//! pane commands: split, kill, select, resize, respawn, send-keys, capture-pane.

use std::path::PathBuf;

use crate::cmd::{
    self, CapturePane, Command, KillPane, ResizeAction, ResizePane, Resolve, RespawnPane,
    SelectPane, SendKeys, SendKeysItem, SendPrefix, SplitWindow,
};
use crate::protocol::ClientCtx;
use crate::server::dispatch::options;
use crate::server::dispatch::{command_argv, resolve_cwd, spawn_pane};
use crate::server::input::{self, parse_prefix};
use crate::server::layout::{Insert, LayoutError};
use crate::server::model::Model;
use crate::server::pane::{PaneHandle, PaneSpawn, usable_cwd};
use crate::server::{State, clients, graphics};
use crate::types::{Dir, Edge, PaneId, Rect, SplitSize, WindowId};

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

/// tmux's default `-F` for `split-window -P` and `new-window -P`.
pub(crate) const NEW_PANE_TEMPLATE: &str = "#{session_name}:#{window_index}.#{pane_index}";

/// `-P`'s output: `format` (else [`NEW_PANE_TEMPLATE`]) expanded for `pane`,
/// on a line of its own.
pub(crate) fn print_pane(model: &Model, pane: PaneId, format: Option<&str>) -> String {
    let format = format.unwrap_or(NEW_PANE_TEMPLATE);
    let mut out = cmd::expand(format, &options::FormatCtx::new(model, pane));
    out.push('\n');
    out
}

/// `split-window`: a new pane next to the target, which becomes active
/// unless `-d`. `-b` puts it before the target (left or above), `-f`
/// beside the whole window, and in index order it goes as tmux's
/// `window_add_pane` puts it: before the target with `-b`, first with
/// `-b -f`, last with `-f`, else after the target. `-P` prints it. `-c`
/// is expanded against the target pane now (spec §4.5).
fn split_window(state: &mut State, ctx: &ClientCtx, c: SplitWindow) -> Result<String, String> {
    let model = &state.model;
    let target = model.resolve_pane(&c.target, ctx)?;
    let p = &model.panes[&target];
    let window = p.window;
    let size = (p.cols, p.rows);
    let s = &model.sessions[&model.windows[&window].session];
    let env = s.env.clone();
    let cwd = resolve_cwd(model, ctx, Some(target), c.cwd.as_deref(), &s.cwd);
    let session = s.id;
    let cell_px = clients::cell_px(state, session);
    let how = Insert {
        dir: c.dir,
        size: c.size,
        before: c.before,
        full: c.full,
    };

    // Check for room on a copy first, so a command is never started for a
    // pane that cannot be placed.
    let w = &model.windows[&window];
    let mut trial = w.layout.clone();
    trial
        .insert(w.area(), target, PaneId(u32::MAX), how)
        .map_err(|e| match e {
            // tmux 3.7b's text, verbatim.
            LayoutError::TooSmall(_) => "size or position no space for a new pane".to_string(),
            LayoutError::NotFound(_) | LayoutError::Invalid => e.to_string(),
        })?;

    unzoom(&mut state.model, window);
    let new = spawn_pane(
        state,
        (session, window),
        env,
        command_argv(c.command),
        cwd,
        size,
        cell_px,
    )?;
    let model = &mut state.model;
    let Some(w) = model.windows.get_mut(&window) else {
        model.panes.remove(&new);
        return Err(format!("can't find window: {window}"));
    };
    if let Err(e) = w.layout.insert(w.area(), target, new, how) {
        model.panes.remove(&new);
        return Err(e.to_string());
    }
    let order = w.pane_ids();
    let at = match (c.before, c.full) {
        (true, true) => Some(0),
        (true, false) => order.iter().position(|p| *p == target),
        (false, true) => Some(order.len() - 1),
        (false, false) => None,
    };
    if let Some(at) = at {
        w.layout.reorder(new, at);
    }
    if !c.detached {
        w.set_active(new);
    }
    model.relayout(window);
    Ok(if c.print {
        print_pane(model, new, c.format.as_deref())
    } else {
        String::new()
    })
}

fn kill_pane(state: &mut State, ctx: &ClientCtx, c: KillPane) -> Result<String, String> {
    let model = &mut state.model;
    let pane = model.resolve_pane(&c.target, ctx)?;
    let window = model.panes[&pane].window;
    unzoom(model, window);
    let removed = model.remove_pane(pane);
    state.events.panes_killed(removed);
    Ok(String::new())
}

/// `select-pane`: the target, or its neighbour across `-L/-R/-U/-D`, or
/// with `-l` the window's last pane ([`select_last`]), becomes its
/// window's active pane. Past the window's edge it wraps around, as tmux
/// does. Changing the active pane leaves zoom, unless `-Z` keeps the
/// window zoomed on the new one.
fn select_pane(state: &mut State, ctx: &ClientCtx, c: SelectPane) -> Result<String, String> {
    let model = &mut state.model;
    if c.last {
        let window = model.resolve_window(&c.target, ctx)?;
        select_last(model, window, c.keep_zoom)?;
        return Ok(String::new());
    }
    let pane = model.resolve_pane(&c.target, ctx)?;
    let window = model.panes[&pane].window;
    let Some(edge) = c.edge else {
        if let Some(title) = &c.title {
            set_title(state, pane, title);
        } else {
            switch_active(model, window, pane, c.keep_zoom);
        }
        return Ok(String::new());
    };
    let Some(w) = model.windows.get(&window) else {
        return Ok(String::new());
    };
    // Neighbours are found in the unzoomed layout.
    let area = w.area();
    let next = w
        .layout
        .neighbour(area, pane, edge)
        .or_else(|| wrapped_neighbour(w.layout.compute(area), area, pane, edge));
    match (next, &c.title) {
        (Some(next), Some(title)) => set_title(state, next, title),
        (None, Some(_)) => {}
        (Some(next), None) => switch_active(model, window, next, c.keep_zoom),
        (None, None) => unzoom(model, window),
    }
    Ok(String::new())
}

/// `select-pane -T`: `title`, expanded against `pane`, becomes its title,
/// as if the pane had set it (OSC 2), so `#{pane_title}`, the status line
/// and `set-titles` all see it.
fn set_title(state: &mut State, pane: PaneId, title: &str) {
    let title = crate::cmd::expand(title, &options::FormatCtx::new(&state.model, pane));
    crate::server::passthrough::on_pane_event(
        state,
        pane,
        crate::server::pane::PaneEventKind::Title(title),
    );
    clients::mark_all_dirty(state);
}

/// `last-pane` and `select-pane -l`: the window's last pane becomes active
/// again (with `keep_zoom`, a zoomed window stays zoomed on it). With no
/// last pane, a window of two panes takes the other one, as tmux 3.7b does.
pub(crate) fn select_last(
    model: &mut Model,
    window: WindowId,
    keep_zoom: bool,
) -> Result<(), String> {
    let w = model
        .windows
        .get(&window)
        .ok_or_else(|| format!("can't find window: {window}"))?;
    let order = w.pane_ids();
    let other = || {
        let pos = order.iter().position(|p| *p == w.active)?;
        pos.checked_sub(1)
            .map(|i| order[i])
            .or_else(|| order.get(pos + 1).copied())
    };
    let last = match w.last_pane() {
        Some(last) => last,
        None if order.len() == 2 => other().ok_or_else(|| "no last pane".to_string())?,
        None => return Err("no last pane".to_string()),
    };
    switch_active(model, window, last, keep_zoom);
    Ok(())
}

/// Makes `pane` its window's active pane. As tmux 3.7b: selecting the
/// active pane changes nothing (zoom included); otherwise the window
/// leaves zoom, or with `keep_zoom` zooms the new active pane instead.
fn switch_active(model: &mut Model, window: WindowId, pane: PaneId, keep_zoom: bool) {
    let Some(w) = model.windows.get(&window) else {
        return;
    };
    if w.active == pane {
        return;
    }
    let zoomed = w.zoomed;
    unzoom(model, window);
    if let Some(w) = model.windows.get_mut(&window) {
        w.set_active(pane);
    }
    if zoomed && keep_zoom {
        toggle_zoom(model, window, pane);
    }
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

/// `resize-pane`. As tmux: `-x`/`-y` first make the pane that wide or
/// tall (cells, or a percent of the window), then `-L n` moves the border
/// on the pane's right left by `n` cells, or the border on its left when
/// it has no right neighbour (`-R`, `-U`, `-D` likewise). `-Z` toggles
/// zoom.
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
    let cells = |size: SplitSize, len: u16| match size {
        SplitSize::Cells(n) => n,
        SplitSize::Percent(p) => (u32::from(len) * u32::from(p) / 100) as u16,
        SplitSize::Half => len / 2,
    };
    if let Some(width) = c.width {
        let cells = cells(width, area.cols);
        w.layout.resize_to(area, pane, Dir::Horizontal, cells);
    }
    if let Some(height) = c.height {
        let cells = cells(height, area.rows);
        w.layout.resize_to(area, pane, Dir::Vertical, cells);
    }
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
    w.set_active(pane);
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
pub(crate) fn unzoom(model: &mut Model, window: WindowId) {
    if let Some(w) = model.windows.get_mut(&window)
        && w.zoomed
    {
        w.zoomed = false;
        model.relayout(window);
    }
}

/// `respawn-pane`: a new process in the same pane (same `%N` and
/// `pane_uid`, spec §4.2), running the given command or the pane's resume
/// argv, in the pane's spawn cwd or `-c` (expanded against the pane). A live
/// pane needs `-k`.
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
            index + model.pane_base_index(w.id)
        ));
    }
    let cwd = c
        .cwd
        .as_deref()
        .map(|dir| resolve_cwd(model, ctx, Some(pane), Some(dir), &p.cwd));
    respawn(state, pane, c.command, cwd).map_err(|e| format!("respawn pane failed: {e}"))?;
    Ok(String::new())
}

/// Starts a new process in `pane` (kept in place, same `%N` and
/// `pane_uid`): `command`, else the pane's resume argv, in `cwd` (`-c`,
/// which becomes the pane's spawn cwd) or else its spawn cwd. A live
/// process is hung up. The error is the reason the spawn failed.
pub(crate) fn respawn(
    state: &mut State,
    pane: PaneId,
    command: Option<Vec<String>>,
    cwd: Option<PathBuf>,
) -> Result<(), String> {
    let model = &state.model;
    let p = model
        .panes
        .get(&pane)
        .ok_or_else(|| format!("can't find pane: {pane}"))?;
    let argv = match command {
        Some(command) => command_argv(Some(command)),
        None => p.resume.clone(),
    };
    let session = model.windows[&p.window].session;
    let spec = PaneSpawn {
        id: pane,
        session,
        argv: argv.clone(),
        cwd: usable_cwd(cwd.unwrap_or_else(|| p.cwd.clone())),
        env: super::pane_env(state, session, pane, model.sessions[&session].env.clone()),
        cols: p.cols,
        rows: p.rows,
        history_limit: model.history_limit(model.windows[&p.window].session),
        socket: state.socket.clone(),
        term: state.inner_term,
        cell_px: clients::cell_px(state, model.windows[&p.window].session),
        graphics_ok: graphics::panes_graphics_ok(state),
    };
    let cwd = spec.cwd.clone();
    let handle = PaneHandle::spawn(spec, state.pane_events.clone(), state.wake.clone())
        .map_err(|e| format!("can't start pane in {}: {e}", cwd.display()))?;
    if let Some(p) = state.model.panes.get_mut(&pane) {
        // Dropping the old handle closes its master, which hangs up a live
        // process (`-k`), and stops its tasks. An `Exited` its reader had
        // already queued carries the old handle's generation, which the
        // actor ignores.
        p.handle = handle;
        p.dead = None;
        p.cwd = cwd;
        p.argv = argv.clone();
        p.resume = argv;
    }
    Ok(())
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

/// `send-prefix`: the target pane's session `prefix` key, typed into the
/// pane as encoded for its input modes. No prefix (`None`) sends nothing.
pub fn send_prefix(state: &mut State, ctx: &ClientCtx, c: SendPrefix) -> Result<String, String> {
    let model = &state.model;
    let pane = model.resolve_pane(&c.target, ctx)?;
    let prefix = model
        .options
        .lookup("prefix", &options::chain(model, pane))
        .and_then(|v| parse_prefix(v).ok())
        .flatten();
    let Some(key) = prefix else {
        return Ok(String::new());
    };
    let handle = &model.panes[&pane].handle;
    let bytes = input::encode(&key, &handle.modes());
    if bytes.is_empty() {
        return Ok(String::new());
    }
    handle
        .enqueue_input(bytes)
        .map_err(|e| format!("send-prefix: {pane}: {e}"))?;
    Ok(String::new())
}

/// `capture-pane`: the pane's text between `-S` and `-E`.
// ponytail: without `-p` tmux fills a paste buffer; there are no buffers
// before M2, so the text is printed either way.
fn capture_pane(state: &mut State, ctx: &ClientCtx, c: CapturePane) -> Result<String, String> {
    let pane = state.model.resolve_pane(&c.target, ctx)?;
    Ok(state.model.panes[&pane]
        .handle
        .capture_lines(&c.opts, c.join))
}
