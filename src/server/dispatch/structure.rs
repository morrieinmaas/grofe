//! Structure verbs (TC.8, M36-R6): moving, swapping, breaking and
//! rotating panes and windows, resizing windows, layout cycling and
//! `respawn-window`. Each follows tmux 3.7b's `cmd-*.c`, checked against
//! `list-panes -F` geometry recorded on a private tmux 3.7b socket.
//!
//! A pane's index is its position in its window's pane list
//! ([`LayoutTree`]'s order), and a window's place in its session is its
//! index, as tmux's winlinks are.

use crate::cmd::{
    BreakPane, JoinPane, LastPane, MovePane, MoveWindow, NextLayout, ResizeWindow, Resolve,
    RespawnPane, RotateWindow, SwapPane, SwapWindow, Target, WindowRef,
};
use crate::protocol::ClientCtx;
use crate::server::State;
use crate::server::dispatch::options::Owner;
use crate::server::dispatch::pane::{respawn, select_last, unzoom};
use crate::server::dispatch::resolve_cwd;
use crate::server::dispatch::window::{AUTOMATIC_RENAME, apply_preset, default_name, main_sizes};
use crate::server::layout::{Insert, LayoutTree, Preset};
use crate::server::model::{HOLE, Model, Window};
use crate::types::{Dir, PaneId, SessionId, SplitSize, WindowId, cap_size};

/// A placeholder id for a pane being moved within its own window, which
/// is in the tree twice for a moment (tmux splits before it closes).
const MOVING: PaneId = PaneId(u32::MAX);

/// `join-pane`.
pub fn join_pane(state: &mut State, ctx: &ClientCtx, c: JoinPane) -> Result<String, String> {
    join(state, ctx, c)
}

/// `move-pane`: tmux's other name for `join-pane`.
pub fn move_pane(state: &mut State, ctx: &ClientCtx, c: MovePane) -> Result<String, String> {
    join(state, ctx, c)
}

/// `join-pane` / `move-pane`: pane `-s` moves next to pane `-t`, which is
/// split as `split-window` would split it (`-h`, `-v`, `-l`, `-b`, `-f`).
/// As tmux 3.7b: the target is split before the source leaves its cell,
/// the source comes right after the target in index order even with `-b`,
/// and without `-d` it becomes the active pane of the current window. A
/// window left with no panes goes.
fn join(state: &mut State, ctx: &ClientCtx, c: JoinPane) -> Result<String, String> {
    let model = &mut state.model;
    let dst = model.resolve_pane(&c.dst, ctx)?;
    let src = model.resolve_pane(&c.src, ctx)?;
    let (dst_w, src_w) = (model.panes[&dst].window, model.panes[&src].window);
    unzoom(model, dst_w);
    unzoom(model, src_w);
    if src == dst {
        return Err("source and target panes must be different".to_string());
    }
    let dir = if c.horizontal {
        Dir::Horizontal
    } else {
        Dir::Vertical
    };
    let w = &model.windows[&dst_w];
    let area = w.area();
    // tmux reads `-l N%` against the target pane (the window with `-f`).
    let across = match (c.full, dir) {
        (true, Dir::Horizontal) => area.cols,
        (true, Dir::Vertical) => area.rows,
        (false, Dir::Horizontal) => model.panes[&dst].cols,
        (false, Dir::Vertical) => model.panes[&dst].rows,
    };
    let size = match c.size {
        None => SplitSize::Half,
        Some(SplitSize::Percent(p)) => {
            SplitSize::Cells((u32::from(across) * u32::from(p) / 100) as u16)
        }
        Some(size) => size,
    };
    let how = Insert {
        dir,
        size,
        before: c.before,
        full: c.full,
    };
    if w.floating(dst).is_some() {
        // tmux 3.7b's text, recorded.
        return Err("size or position can't split a floating pane".to_string());
    }
    let same = src_w == dst_w;
    let mut layout = w.layout.clone();
    let placed = if same { MOVING } else { src };
    layout
        .insert(area, dst, placed, how)
        .map_err(|_| "size or position no space for a new pane".to_string())?;

    if same {
        let Some(w) = model.windows.get_mut(&dst_w) else {
            return Ok(String::new());
        };
        let order = w.pane_ids();
        layout.remove(src);
        layout.replace(MOVING, src);
        w.layout = layout;
        // A floating pane joined into the layout is tiled from now on.
        w.floating.retain(|f| f.pane != src);
        w.lose_pane(&order, src);
    } else {
        take_out(model, src_w, src);
        if let Some(w) = model.windows.get_mut(&dst_w) {
            w.layout = layout;
        }
        if let Some(p) = model.panes.get_mut(&src) {
            p.window = dst_w;
        }
    }
    if !c.detach
        && let Some(w) = model.windows.get_mut(&dst_w)
    {
        w.set_active(src);
        model.select_window(dst_w);
    }
    finish_leaving(model, src_w, dst_w);
    Ok(String::new())
}

/// Takes `pane` out of `window`'s layout without killing it ([`Window::lose_pane`]).
/// A pane that is the window's only one stays in the layout;
/// [`finish_leaving`] closes the window.
fn take_out(model: &mut Model, window: WindowId, pane: PaneId) {
    let Some(w) = model.windows.get_mut(&window) else {
        return;
    };
    let order = w.pane_ids();
    if order.len() < 2 {
        return;
    }
    if w.floating(pane).is_some() {
        w.floating.retain(|f| f.pane != pane);
    } else if w.tiled_ids().len() < 2 {
        w.layout.replace(pane, HOLE);
    } else {
        w.layout.remove(pane);
    }
    w.lose_pane(&order, pane);
}

/// After a pane left `from` for `to`: `from` goes if it has no pane left,
/// and both are laid out again.
fn finish_leaving(model: &mut Model, from: WindowId, to: WindowId) {
    let empty = from != to && !model.panes.values().any(|p| p.window == from);
    if empty {
        // No pane of it is left, so this kills nothing.
        model.remove_window(from);
    } else {
        model.relayout(from);
    }
    model.relayout(to);
}

/// `swap-pane`: `-s` (the current pane) and `-t` trade places, or with
/// `-U`/`-D` the target and the pane before or after it (wrapping). Each
/// takes the other's cell and index. As tmux 3.7b: the target pane becomes
/// active in the window both are in; across windows, each window's
/// incoming pane becomes its active one.
pub fn swap_pane(state: &mut State, ctx: &ClientCtx, c: SwapPane) -> Result<String, String> {
    let model = &mut state.model;
    let dst = model.resolve_pane(&c.dst, ctx)?;
    let dst_w = model.panes[&dst].window;
    let src = if c.up || c.down {
        let order = model.windows[&dst_w].pane_ids();
        let pos = order.iter().position(|p| *p == dst).unwrap_or(0);
        let n = order.len();
        order[if c.down {
            (pos + 1) % n
        } else {
            (pos + n - 1) % n
        }]
    } else {
        model.resolve_pane(c.src.as_ref().unwrap_or(&Target::Current), ctx)?
    };
    let src_w = model.panes[&src].window;
    let floats = |w: WindowId, p: PaneId| model.windows[&w].floating(p).is_some();
    if floats(src_w, src) || floats(dst_w, dst) {
        // tmux 3.7b's text, recorded.
        return Err("cannot swap floating panes".to_string());
    }
    unzoom(model, dst_w);
    unzoom(model, src_w);
    if src == dst {
        return Ok(String::new());
    }
    if src_w == dst_w {
        let Some(w) = model.windows.get_mut(&dst_w) else {
            return Ok(String::new());
        };
        w.layout.swap(src, dst);
        w.set_active(dst);
        model.relayout(dst_w);
        return Ok(String::new());
    }
    for (window, out, into) in [(src_w, src, dst), (dst_w, dst, src)] {
        if let Some(w) = model.windows.get_mut(&window) {
            w.layout.replace(out, into);
            w.set_active(into);
            // The pane that left is no one's last pane here.
            w.last_panes.retain(|p| *p != out);
        }
        if let Some(p) = model.panes.get_mut(&into) {
            p.window = window;
        }
    }
    model.relayout(src_w);
    model.relayout(dst_w);
    Ok(String::new())
}

/// A window's place: its session and index (tmux's winlink).
fn slot(model: &Model, window: WindowId) -> (SessionId, u32) {
    let w = &model.windows[&window];
    (w.session, w.index)
}

/// `swap-window`: windows `-s` (the current one) and `-t` trade places,
/// in the same session or two. As tmux 3.7b: a session's current and last
/// windows are places, so whichever window is now in its current place is
/// current.
pub fn swap_window(state: &mut State, ctx: &ClientCtx, c: SwapWindow) -> Result<String, String> {
    let model = &mut state.model;
    let dst = model.resolve_window(&c.dst, ctx)?;
    let src = model.resolve_window(c.src.as_ref().unwrap_or(&Target::Current), ctx)?;
    if src == dst {
        return Ok(String::new());
    }
    let ((src_s, src_i), (dst_s, dst_i)) = (slot(model, src), slot(model, dst));
    let swap = |id: &mut WindowId| {
        if *id == src {
            *id = dst;
        } else if *id == dst {
            *id = src;
        }
    };
    for session in [src_s, dst_s] {
        if let Some(s) = model.sessions.get_mut(&session) {
            s.windows.iter_mut().for_each(swap);
            s.last_windows.iter_mut().for_each(swap);
            swap(&mut s.active);
            if src_s == dst_s {
                break;
            }
        }
    }
    if let Some(w) = model.windows.get_mut(&src) {
        (w.session, w.index) = (dst_s, dst_i);
    }
    if let Some(w) = model.windows.get_mut(&dst) {
        (w.session, w.index) = (src_s, src_i);
    }
    Ok(String::new())
}

/// `break-pane`: pane `-s` moves into a new window of its own, at `-t`'s
/// index or the session's first free one, named `-n` (which turns
/// `automatic-rename` off for it, as tmux does). It becomes current unless
/// `-d`. A pane alone in its window moves its window instead, as in tmux.
pub fn break_pane(state: &mut State, ctx: &ClientCtx, c: BreakPane) -> Result<String, String> {
    let model = &mut state.model;
    let pane = model.resolve_pane(&c.src, ctx)?;
    let from = model.panes[&pane].window;
    unzoom(model, from);
    let (session, index) = destination(model, ctx, c.dst.as_ref(), false)?;
    let alone = model.windows[&from].pane_ids().len() == 1;
    let index = match index {
        Some(i) => match model.window_at_index(session, i) {
            Some(w) if alone && w == from => return Err(format!("same index: {i}")),
            Some(_) => return Err(format!("index in use: {i}")),
            None => i,
        },
        None => model.free_window_index(session),
    };
    let window = if alone {
        relink(model, from, session, index);
        from
    } else {
        let (cols, rows) = (model.windows[&from].cols, model.windows[&from].rows);
        take_out(model, from, pane);
        let id = model.alloc_window_id();
        let name = default_name(&model.panes[&pane].cwd);
        model.insert_window(Window {
            id,
            session,
            index,
            name,
            layout: LayoutTree::new(pane),
            active: pane,
            zoomed: false,
            cols,
            rows,
            bell_flag: false,
            activity_flag: false,
            silence_flag: false,
            last_panes: Vec::new(),
            floating: Vec::new(),
            pane_status: Default::default(),
        });
        if let Some(p) = model.panes.get_mut(&pane) {
            p.window = id;
        }
        finish_leaving(model, from, id);
        id
    };
    if let Some(name) = c.name {
        model
            .options
            .set(Owner::Window(window), AUTOMATIC_RENAME, "off", false)?;
        if let Some(w) = model.windows.get_mut(&window) {
            w.name = name;
        }
    }
    if !c.detach {
        model.select_window(window);
    }
    Ok(String::new())
}

/// Where a window goes: `-t`'s session and index (`None`: the first free
/// one), the current session without `-t`. With `after` (`-a`), the index
/// after `-t`'s window.
fn destination(
    model: &Model,
    ctx: &ClientCtx,
    dst: Option<&Target>,
    after: bool,
) -> Result<(SessionId, Option<u32>), String> {
    let Some(dst) = dst else {
        let session = model.resolve_session(&Target::Current, ctx)?;
        if after {
            let current = model.sessions[&session].active;
            return Ok((session, Some(next_index(model.windows[&current].index)?)));
        }
        return Ok((session, None));
    };
    let dst = &model.window_index_type(dst, ctx)?;
    let (session, index) = match dst {
        Target::Window(s, WindowRef::Index(i)) => (
            model.resolve_session(&Target::Session(s.clone()), ctx)?,
            Some(*i),
        ),
        Target::Session(_) | Target::Window(_, WindowRef::Current) if !after => {
            (model.resolve_session(dst, ctx)?, None)
        }
        _ => {
            let window = model.resolve_window(dst, ctx)?;
            let (session, index) = slot(model, window);
            (session, Some(index))
        }
    };
    let index = match index {
        Some(i) if after => Some(next_index(i)?),
        other => other,
    };
    Ok((session, index))
}

/// `move-window`: window `-s` (the current one) moves to `-t`'s index, or
/// the first free index of `-t`'s session, and becomes current there. With
/// `-a`, to the index after `-t`'s window, moving later windows up to make
/// room. With `-r`, the windows of session `-t` are renumbered from
/// `base-index` in order instead. As tmux 3.7b.
pub fn move_window(state: &mut State, ctx: &ClientCtx, c: MoveWindow) -> Result<String, String> {
    let model = &mut state.model;
    if c.renumber {
        let session = model.resolve_session(c.dst.as_ref().unwrap_or(&Target::Current), ctx)?;
        let base = model.base_index(session);
        for (i, id) in model.sessions[&session].windows.clone().iter().enumerate() {
            if let Some(w) = model.windows.get_mut(id) {
                w.index = base + i as u32;
            }
        }
        return Ok(String::new());
    }
    let window = model.resolve_window(&c.src, ctx)?;
    let (session, index) = destination(model, ctx, c.dst.as_ref(), c.after)?;
    let index = match index {
        None => model.free_window_index(session),
        Some(i) if c.after => {
            shuffle_up(model, session, i)?;
            i
        }
        Some(i) => match model.window_at_index(session, i) {
            Some(w) if w == window => return Err(format!("same index: {i}")),
            Some(_) => return Err(format!("index in use: {i}")),
            None => i,
        },
    };
    relink(model, window, session, index);
    model.select_window(window);
    Ok(String::new())
}

/// Moves `window` to `index` of `session` (which is free there): out of
/// its session's window list ([`unlink`]) and into the new one's.
fn relink(model: &mut Model, window: WindowId, session: SessionId, index: u32) {
    let Some(w) = model.windows.get(&window) else {
        return;
    };
    if w.session == session {
        // Within its session only its place in the list changes; tmux
        // links it again before unlinking, so the session never empties.
        if let Some(s) = model.sessions.get_mut(&session) {
            s.windows.retain(|id| *id != window);
        }
    } else {
        unlink(model, window);
    }
    let Some(w) = model.windows.get_mut(&window) else {
        return;
    };
    (w.session, w.index) = (session, index);
    let windows = &model.windows;
    if let Some(s) = model.sessions.get_mut(&session) {
        let pos = s
            .windows
            .iter()
            .position(|id| windows.get(id).is_some_and(|w| w.index > index))
            .unwrap_or(s.windows.len());
        s.windows.insert(pos, window);
    }
}

/// tmux's `winlink_shuffle_up`: frees `index` in `session` by moving the
/// windows from it up to the first free index up by one.
pub(crate) fn shuffle_up(model: &mut Model, session: SessionId, index: u32) -> Result<(), String> {
    let Some(gap) = (index..=u32::MAX).find(|i| model.window_at_index(session, *i).is_none())
    else {
        return Err(format!("no free index after {index}"));
    };
    for i in (index..gap).rev() {
        if let Some(id) = model.window_at_index(session, i)
            && let Some(w) = model.windows.get_mut(&id)
        {
            // `i` is below `gap`, so this cannot overflow.
            w.index = i + 1;
        }
    }
    Ok(())
}

/// The index after `index`, or an error past the largest.
pub(crate) fn next_index(index: u32) -> Result<u32, String> {
    index
        .checked_add(1)
        .ok_or_else(|| format!("no index after {index}"))
}

/// Takes `window` out of its session's window list without closing it
/// ([`Model::unlink_window`]).
fn unlink(model: &mut Model, window: WindowId) {
    if let Some(session) = model.windows.get(&window).map(|w| w.session) {
        model.unlink_window(session, window);
    }
}

/// `resize-window`: the window becomes `-x` by `-y` cells (either kept
/// when not given), or with `-A`/`-a` the size of the largest or smallest
/// client showing its session (its terminal less the status row; the
/// session's size with none), and its panes are laid out again. As tmux
/// 3.7b, it sets the window's `window-size` to `manual`, `-A`/`-a`
/// included, so clients no longer resize it.
pub fn resize_window(
    state: &mut State,
    ctx: &ClientCtx,
    c: ResizeWindow,
) -> Result<String, String> {
    let window = state.model.resolve_window(&c.target, ctx)?;
    let session = state.model.windows[&window].session;
    let status = state
        .model
        .options
        .lookup("status", &[Owner::Session(session)])
        .is_some_and(|v| v != "off");
    let clients = state
        .clients
        .values()
        .filter(|cl| cl.session == session)
        .map(|cl| (cl.size.0, cl.size.1.saturating_sub(u16::from(status))));
    let area = |(cols, rows): &(u16, u16)| u32::from(*cols) * u32::from(*rows);
    let by_client = if c.largest {
        Some(clients.max_by_key(area))
    } else if c.smallest {
        Some(clients.min_by_key(area))
    } else {
        None
    };
    let model = &mut state.model;
    let s = &model.sessions[&session];
    let session_size = (s.cols, s.rows);
    model
        .options
        .set(Owner::Window(window), "window-size", "manual", false)?;
    let Some(w) = model.windows.get_mut(&window) else {
        return Ok(String::new());
    };
    let (cols, rows) = match by_client {
        Some(size) => size.unwrap_or(session_size),
        None => (c.width.unwrap_or(w.cols), c.height.unwrap_or(w.rows)),
    };
    (w.cols, w.rows) = cap_size(cols, rows);
    let (zoomed, active, size) = (w.zoomed, w.active, (w.cols, w.rows));
    model.relayout(window);
    if zoomed && let Some(p) = model.panes.get_mut(&active) {
        (p.cols, p.rows) = size;
        if let Err(e) = p.handle.resize(size.0, size.1) {
            eprintln!("grofe server: resizing {active}: {e}");
        }
    }
    Ok(String::new())
}

/// `last-pane`: the window's previously active pane becomes active again
/// (leaving zoom), as tmux does ([`select_last`]).
pub fn last_pane(state: &mut State, ctx: &ClientCtx, c: LastPane) -> Result<String, String> {
    let model = &mut state.model;
    let window = model.resolve_window(&c.target, ctx)?;
    select_last(model, window, false)?;
    Ok(String::new())
}

/// `rotate-window`: every pane moves one index position up (`-U`, the
/// default) or down (`-D`), taking over that position's cell. The active
/// pane changes so that the same cell stays active, as in tmux 3.7b.
pub fn rotate_window(
    state: &mut State,
    ctx: &ClientCtx,
    c: RotateWindow,
) -> Result<String, String> {
    let model = &mut state.model;
    let window = model.resolve_window(&c.target, ctx)?;
    unzoom(model, window);
    let Some(w) = model.windows.get_mut(&window) else {
        return Ok(String::new());
    };
    let order = w.pane_ids();
    let pos = order.iter().position(|p| *p == w.active);
    if w.floating.is_empty() {
        w.layout.rotate(c.down);
    } else {
        rotate_slots(w, &order, c.down);
    }
    if let Some(next) = pos.and_then(|i| w.pane_ids().get(i).copied()) {
        w.set_active(next);
    }
    model.relayout(window);
    Ok(String::new())
}

/// rotate-window over a window with floating panes, as tmux 3.7b does
/// (recorded): the panes in index order move one place up (`-D`: down)
/// through the window's slots, the layout's cells then the floating
/// panes' places, so a tiled pane can land in a floating place (and float
/// there, at that place's size) and a floating pane in a cell.
fn rotate_slots(w: &mut Window, order: &[PaneId], down: bool) {
    let n = order.len();
    if n < 2 {
        return;
    }
    let moved: Vec<PaneId> = (0..n)
        .map(|i| order[if down { (i + n - 1) % n } else { (i + 1) % n }])
        .collect();
    let tiled = w.tiled_ids();
    // Through placeholders, since a cell may not name a pane twice.
    let placeholder = |i: usize| PaneId(HOLE.0 - 1 - i as u32);
    for (i, pane) in tiled.iter().enumerate() {
        w.layout.replace(*pane, placeholder(i));
    }
    for (i, pane) in moved.iter().take(tiled.len()).enumerate() {
        w.layout.replace(placeholder(i), *pane);
    }
    for (f, pane) in w.floating.iter_mut().zip(&moved[tiled.len()..]) {
        f.pane = *pane;
    }
}

/// `next-layout`: the preset after the last one applied to the window
/// (the first, `even-horizontal`, if none was), in tmux's order.
pub fn next_layout(state: &mut State, ctx: &ClientCtx, c: NextLayout) -> Result<String, String> {
    let model = &mut state.model;
    // tmux finds `-t` as a pane (cmd-select-layout.c).
    let window = model.resolve_window(&model.pane_type(&c.target, ctx)?, ctx)?;
    unzoom(model, window);
    let (width, height) = main_sizes(model, window);
    let next = model.windows[&window]
        .layout
        .last_preset()
        .map_or(0, |i| (i + 1) % Preset::NAMES.len());
    let preset = Preset::from_name(Preset::NAMES[next], width, height)
        .ok_or_else(|| format!("invalid layout: {}", Preset::NAMES[next]))?;
    apply_preset(model, window, preset);
    Ok(String::new())
}

/// `respawn-window`: the window keeps its first pane only (the others are
/// killed) and starts it again with the given command or its own, as tmux
/// does. A window with a live pane needs `-k`.
pub fn respawn_window(
    state: &mut State,
    ctx: &ClientCtx,
    c: RespawnPane,
) -> Result<String, String> {
    let model = &mut state.model;
    let window = model.resolve_window(&c.target, ctx)?;
    let panes = model.windows[&window].pane_ids();
    let live = panes
        .iter()
        .any(|p| model.panes.get(p).is_some_and(|p| p.dead.is_none()));
    if live && !c.kill {
        let w = &model.windows[&window];
        return Err(format!(
            "respawn window failed: window {}:{} still active",
            model.sessions[&w.session].name, w.index
        ));
    }
    unzoom(model, window);
    let Some((&first, rest)) = panes.split_first() else {
        return Ok(String::new());
    };
    let cwd = c.cwd.as_deref().map(|dir| {
        let spawned = &model.panes[&first].cwd;
        resolve_cwd(model, ctx, Some(first), Some(dir), spawned)
    });
    for pane in rest {
        let removed = state.model.remove_pane(*pane);
        state.events.panes_killed(removed);
    }
    respawn(state, first, c.command, cwd).map_err(|e| format!("respawn window failed: {e}"))?;
    if let Some(w) = state.model.windows.get_mut(&window) {
        w.set_active(first);
    }
    state.model.relayout(window);
    Ok(String::new())
}

#[cfg(test)]
mod tests {
    use crate::server::{dispatch, test_ctx, test_state};

    /// Every verb given bare answers, and none says `not implemented`.
    #[test]
    fn every_verb_is_implemented() {
        let mut state = test_state();
        let ctx = test_ctx();
        for verb in [
            "join-pane",
            "swap-pane",
            "swap-window",
            "break-pane",
            "resize-window",
            "last-pane",
            "rotate-window",
            "move-window",
            "move-pane",
            "next-layout",
            "respawn-window",
        ] {
            let got = dispatch::run(&mut state, &ctx, &[verb.to_string()]);
            assert_ne!(got, Err(format!("not implemented: {verb}")), "{verb}");
        }
    }
}
