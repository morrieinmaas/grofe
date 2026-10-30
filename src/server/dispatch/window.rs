//! window commands, plus `list-panes` in its default format.

use std::path::Path;

use crate::cmd::{
    self, KillWindow, ListPanes, ListWindows, NewWindow, Rename, Resolve, SelectLayout,
    SelectWindow, Target, WindowMove, WindowRef, WindowStep,
};
use crate::protocol::ClientCtx;
use crate::server::dispatch::options::{self, FormatCtx, Owner};
use crate::server::dispatch::pane::{print_pane, unzoom};
use crate::server::dispatch::structure::{next_index, shuffle_up};
use crate::server::dispatch::{resolve_cwd, spawn_pane};
use crate::server::layout::{LayoutTree, MainSize, Preset};
use crate::server::model::{Model, Window};
use crate::server::{State, clients};
use crate::types::{SessionId, WindowId};

pub(crate) const AUTOMATIC_RENAME: &str = "automatic-rename";

/// A new window's name when `-n` is not given: the basename of its cwd,
/// until [`automatic_rename`] names it from its pane.
pub fn default_name(cwd: &Path) -> String {
    cwd.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| cwd.display().to_string())
}

/// `new-window`: `-t session:N` creates window N (which must be free);
/// any other target picks the session and the lowest free index. With
/// `-a`, the index after the target window, later windows moving up as
/// tmux's `winlink_shuffle_up` moves them. `-P` prints the new pane.
pub fn new_window(state: &mut State, ctx: &ClientCtx, c: NewWindow) -> Result<String, String> {
    let model = &state.model;
    let target = model.window_index_type(&c.target, ctx)?;
    // `-S` with `-n` and no index: a window of that name is selected
    // instead, if the session has one (and two is an error).
    if c.select_existing
        && let Some(name) = &c.name
        && !matches!(target, Target::Window(_, WindowRef::Index(_)))
    {
        let session = model.resolve_session(&target, ctx)?;
        let named: Vec<_> = model.sessions[&session]
            .windows
            .iter()
            .copied()
            .filter(|w| model.windows.get(w).is_some_and(|w| &w.name == name))
            .collect();
        match named[..] {
            [] => {}
            [window] => {
                if !c.detached {
                    state.model.select_window_in(session, window);
                }
                return Ok(String::new());
            }
            _ => return Err(format!("multiple windows named {name}")),
        }
    }
    let relative = c.after || c.before;
    let (session, index) = match &target {
        // -a, -b: after or before the target window, moving later windows
        // up; a free index is taken as it is, as tmux 3.7b does. With both,
        // -b wins.
        target if relative => match model.resolve_winlink(target, ctx) {
            Ok((s, w)) => {
                let at = model.index_in(s, w).unwrap_or(0);
                (s, if c.before { at } else { next_index(at)? })
            }
            Err(e) => match target {
                Target::Window(s, WindowRef::Index(i)) => {
                    (model.resolve_session(&Target::Session(s.clone()), ctx)?, *i)
                }
                _ => return Err(e),
            },
        },
        Target::Window(s, WindowRef::Index(index)) => {
            let session = model.resolve_session(&Target::Session(s.clone()), ctx)?;
            if !c.kill && model.window_at_index(session, *index).is_some() {
                return Err(format!("create window failed: index {index} in use"));
            }
            (session, *index)
        }
        target => {
            let session = model.resolve_session(target, ctx)?;
            (session, model.free_window_index(session))
        }
    };
    // `-k`: a window at the index goes, and a current one is replaced by
    // the new window even with `-d`.
    let mut replaced_current = false;
    if c.kill
        && !relative
        && let Some(old) = state.model.window_at_index(session, index)
    {
        replaced_current = state.model.sessions[&session].active == old;
        state.model.remove_link(session, old);
        let removed = state.model.close_orphans(&[old]);
        state.events.panes_killed(removed);
    }
    // As tmux: the windows in the way move up before the pane starts.
    if relative {
        shuffle_up(&mut state.model, session, index)?;
    }
    let model = &state.model;
    let s = &model.sessions[&session];
    // `-e` wins over the session's own variables.
    let mut env = s.env.clone();
    env.extend(c.env);
    let target = model.windows.get(&s.active).map(|w| w.active);
    let cwd = resolve_cwd(model, ctx, target, c.cwd.as_deref(), &s.cwd);
    let size = (s.cols, s.rows);
    let cell_px = clients::cell_px(state, session);

    let window = state.model.alloc_window_id();
    let pane = spawn_pane(
        state,
        (session, window),
        env,
        c.command,
        cwd.clone(),
        size,
        cell_px,
    )?;
    let model = &mut state.model;
    let named = c.name.is_some();
    model.insert_window(Window {
        id: window,
        session,
        index,
        name: c.name.unwrap_or_else(|| default_name(&cwd)),
        layout: LayoutTree::new(pane),
        active: pane,
        zoomed: false,
        cols: size.0,
        rows: size.1,
        bell_flag: false,
        activity_flag: false,
        silence_flag: false,
        activity: std::time::SystemTime::now(),
        last_panes: Vec::new(),
        active_points: Default::default(),
        floating: Vec::new(),
        pane_status: Default::default(),
        links: Vec::new(),
    });
    // As tmux: a window named by `-n` keeps that name.
    if named {
        stop_automatic_rename(model, window)?;
    }
    if !c.detached || replaced_current {
        model.select_window_in(session, window);
    }
    Ok(if c.print {
        print_pane(model, pane, c.format.as_deref())
    } else {
        String::new()
    })
}

fn stop_automatic_rename(model: &mut Model, window: WindowId) -> Result<(), String> {
    model
        .options
        .set(Owner::Window(window), AUTOMATIC_RENAME, "off", false)
        .map(|_| ())
}

/// `automatic-rename` (spec §4.5): every window with the option on is
/// named by its `automatic-rename-format` (default
/// `#{b:pane_current_path}`), expanded against its active pane. Pane
/// titles play no part, so an OSC title never freezes the name; only
/// `rename-window` (or `new-window -n`) turns the option off. An empty
/// expansion leaves the name alone. Returns whether any name changed.
pub fn automatic_rename(model: &mut Model) -> bool {
    let names: Vec<(WindowId, String)> = model
        .windows
        .values()
        .filter_map(|w| {
            let chain = options::chain(model, w.active);
            if model.options.lookup(AUTOMATIC_RENAME, &chain) != Some("on") {
                return None;
            }
            let format = model
                .options
                .lookup("automatic-rename-format", &chain)
                .unwrap_or_default();
            let name = cmd::expand(format, &FormatCtx::new(model, w.active));
            (!name.is_empty() && name != w.name).then_some((w.id, name))
        })
        .collect();
    let changed = !names.is_empty();
    for (id, name) in names {
        if let Some(w) = model.windows.get_mut(&id) {
            w.name = name;
        }
    }
    changed
}

/// `rename-window`: the name sticks, since it turns `automatic-rename`
/// off for the window, as tmux does.
///
/// In the tmux dialect the name is a format, expanded against the window's
/// active pane, as tmux 3.7b expands it (recorded: `rename-window 'a#Sb'`
/// names the window `amainb`).
pub fn rename_window(state: &mut State, ctx: &ClientCtx, c: Rename) -> Result<String, String> {
    let window = state.model.resolve_window(&c.target, ctx)?;
    let name = match state.model.windows.get(&window) {
        Some(w) if ctx.dialect == crate::protocol::Dialect::Tmux => {
            let vars = FormatCtx::new(&state.model, w.active);
            crate::cmd::expand(&c.name, &options::ServerVars::new(state, w.active, &vars))
        }
        _ => c.name,
    };
    let model = &mut state.model;
    stop_automatic_rename(model, window)?;
    if let Some(w) = model.windows.get_mut(&window) {
        w.name = name;
    }
    Ok(String::new())
}

/// Which way `next-window` / `previous-window` steps.
#[derive(Clone, Copy)]
pub enum Step {
    Next,
    Previous,
}

/// `next-window` / `previous-window`: the target session's next or
/// previous window by index, wrapping around as tmux does. With `-a`, the
/// next or previous one with an alert (a bell, activity or silence flag,
/// `server::alerts`); none is an error, as in tmux 3.7b.
pub fn step_window(
    state: &mut State,
    ctx: &ClientCtx,
    c: WindowStep,
    step: Step,
) -> Result<String, String> {
    let model = &mut state.model;
    let session = model.resolve_session(&c.target, ctx)?;
    step_in(model, session, step, c.alert)?;
    Ok(String::new())
}

fn step_in(model: &mut Model, session: SessionId, step: Step, alert: bool) -> Result<(), String> {
    let s = &model.sessions[&session];
    let n = s.windows.len();
    let none = || {
        match step {
            Step::Next => "no next window",
            Step::Previous => "no previous window",
        }
        .to_string()
    };
    if n < 2 {
        return Err(none());
    }
    let pos = s.windows.iter().position(|w| *w == s.active).unwrap_or(0);
    let at = |k: usize| match step {
        Step::Next => (pos + k) % n,
        Step::Previous => (pos + n - k % n) % n,
    };
    let alerted = |id: &WindowId| {
        model
            .windows
            .get(id)
            .is_some_and(|w| w.bell_flag || w.activity_flag || w.silence_flag)
    };
    let window = if alert {
        (1..n)
            .map(|k| s.windows[at(k)])
            .find(alerted)
            .ok_or_else(none)?
    } else {
        s.windows[at(1)]
    };
    model.select_window_in(session, window);
    Ok(())
}

/// `last-window`: back to the target session's previous window.
pub fn last_window(state: &mut State, ctx: &ClientCtx, c: WindowStep) -> Result<String, String> {
    let model = &mut state.model;
    let session = model.resolve_session(&c.target, ctx)?;
    last_in(model, session)?;
    Ok(String::new())
}

fn last_in(model: &mut Model, session: SessionId) -> Result<(), String> {
    let last = *model.sessions[&session]
        .last_windows
        .last()
        .ok_or_else(|| "no last window".to_string())?;
    model.select_window_in(session, last);
    Ok(())
}

/// `kill-window`; with `-a` every other window of the target's session,
/// the target becoming current (recorded on tmux 3.7b).
pub fn kill_window(state: &mut State, ctx: &ClientCtx, c: KillWindow) -> Result<String, String> {
    let (session, window) = state.model.resolve_winlink(&c.target, ctx)?;
    let doomed = match c.all {
        true => {
            state.model.select_window_in(session, window);
            state.model.sessions[&session]
                .windows
                .iter()
                .copied()
                .filter(|w| *w != window)
                .collect()
        }
        false => vec![window],
    };
    for w in doomed {
        let removed = state.model.remove_window(w);
        state.events.panes_killed(removed);
    }
    Ok(String::new())
}

pub fn select_window(
    state: &mut State,
    ctx: &ClientCtx,
    c: SelectWindow,
) -> Result<String, String> {
    let model = &mut state.model;
    let (session, window) = model.resolve_winlink(&c.target, ctx)?;
    // As tmux's `cmd_select_window_exec`: `-n`, `-p` and `-l` move the
    // target's session, and `-T` on the current window goes back instead.
    match c.step {
        Some(WindowMove::Next) => step_in(model, session, Step::Next, false)?,
        Some(WindowMove::Previous) => step_in(model, session, Step::Previous, false)?,
        Some(WindowMove::Last) => last_in(model, session)?,
        None if c.toggle && model.sessions[&session].active == window => last_in(model, session)?,
        None => model.select_window_in(session, window),
    }
    Ok(String::new())
}

/// `list-windows` in the default format; `-F` is routed to `options`.
pub fn list_windows(state: &State, ctx: &ClientCtx, c: ListWindows) -> Result<String, String> {
    let model = &state.model;
    Ok(options::listed_windows(state, ctx, &c)?
        .iter()
        .filter_map(|(s, id)| Some((model.sessions.get(s)?, model.windows.get(id)?)))
        .map(|(s, w)| {
            let active = w.id == s.active;
            // With `-a`, each line says its session, as tmux's does.
            let session = if c.all {
                format!("{}:", s.name)
            } else {
                String::new()
            };
            format!(
                "{session}{}: {}{} ({} panes) [{}x{}] {}{}\n",
                model.index_in(s.id, w.id).unwrap_or(w.index),
                w.name,
                if active { "*" } else { "" },
                w.pane_ids().len(),
                w.cols,
                w.rows,
                w.id,
                if active { " (active)" } else { "" },
            )
        })
        .collect())
}

/// `list-panes` in the default format; `-F` is routed to `options`.
pub fn list_panes(state: &State, ctx: &ClientCtx, c: ListPanes) -> Result<String, String> {
    let model = &state.model;
    let listed = options::listed_panes(state, ctx, &c)?;
    Ok(listed
        .iter()
        .filter_map(|(s, id)| Some((*s, model.panes.get(id)?)))
        .filter_map(|(s, p)| {
            let w = model.windows.get(&p.window)?;
            let s = model.sessions.get(&s)?;
            let at = w.pane_ids().iter().position(|id| *id == p.id)? as u32;
            Some((s, w, at + model.pane_base_index(w.id), p))
        })
        .map(|(s, w, index, p)| {
            let window_index = model.index_in(s.id, w.id).unwrap_or(w.index);
            // With `-s` each line says its window, with `-a` its session
            // too, as tmux's do.
            let index = match (c.all, c.session) {
                (true, _) => format!("{}:{}.{index}", s.name, window_index),
                (false, true) => format!("{}.{index}", window_index),
                _ => index.to_string(),
            };
            format!(
                "{index}: [{}x{}] {}{}{}\n",
                p.cols,
                p.rows,
                p.id,
                if p.id == w.active { " (active)" } else { "" },
                if p.dead.is_some() { " (dead)" } else { "" },
            )
        })
        .collect())
}

/// `select-layout` (spec §4.3, R18): rebuilds the window's layout as a
/// preset over its panes in index order, the first pane being main, its
/// size read from `main-pane-width` or `main-pane-height`. With `-E` it
/// spreads the target pane's cells out evenly instead. A zoomed window is
/// unzoomed first, even for an unknown name, and the active pane stays, as
/// tmux 3.7b does.
pub fn select_layout(
    state: &mut State,
    ctx: &ClientCtx,
    c: SelectLayout,
) -> Result<String, String> {
    let model = &mut state.model;
    // tmux finds `-t` as a pane (cmd-select-layout.c).
    let window = model.resolve_window(&model.pane_type(&c.target, ctx)?, ctx)?;
    // The window leaves zoom first, whatever happens after.
    unzoom(model, window);
    let was = options::window_layout(&model.windows[&window]);
    let old = model.old_layouts.insert(window, was);
    let step = match (c.next, c.previous) {
        (true, _) => Some(1),
        (false, true) => Some(PRESETS - 1),
        _ => None,
    };
    let result = if let Some(step) = step {
        step_preset(model, window, step);
        Ok(())
    } else if c.spread {
        let pane = model.resolve_pane(&c.target, ctx)?;
        if let Some(w) = model.windows.get_mut(&window) {
            let area = w.area();
            w.layout.spread(area, pane);
        }
        model.relayout(window);
        Ok(())
    } else {
        // The name, else (`-o`) the layout before the last change.
        let name = match (c.name.as_str(), c.undo) {
            ("", true) => old.clone(),
            ("", false) => None,
            (name, _) => Some(name.to_string()),
        };
        let preset = match (&name, c.undo) {
            (_, true) => None,
            (None, false) => model.windows[&window].layout.last_preset(),
            (Some(name), false) => preset_named(name),
        };
        match (preset, name) {
            (Some(index), _) => {
                apply_nth_preset(model, window, index);
                Ok(())
            }
            (None, Some(name)) => layout_from_string(state, window, &name),
            (None, None) => Ok(()),
        }
    };
    if result.is_err() {
        // A failed change leaves the layout it would have undone.
        match old {
            Some(old) => state.model.old_layouts.insert(window, old),
            None => state.model.old_layouts.remove(&window),
        };
    }
    result.map(|()| String::new())
}

/// tmux's presets, in its order.
const PRESETS: usize = Preset::NAMES.len();

/// `layout_set_lookup`: the preset `name` is, whole or as the one preset it
/// starts.
fn preset_named(name: &str) -> Option<usize> {
    if let Some(exact) = Preset::NAMES.iter().position(|n| *n == name) {
        return Some(exact);
    }
    let mut starts = Preset::NAMES
        .iter()
        .enumerate()
        .filter(|(_, n)| n.starts_with(name));
    match (starts.next(), starts.next()) {
        (Some((index, _)), None) => Some(index),
        _ => None,
    }
}

/// Preset number `index` over the window's panes.
fn apply_nth_preset(model: &mut Model, window: WindowId, index: usize) {
    let (width, height) = main_sizes(model, window);
    let columns = tiled_max_columns(model, window);
    if let Some(preset) = Preset::from_name(Preset::NAMES[index], width, height, columns) {
        apply_preset(model, window, preset);
    }
}

/// The preset `step` after the last one applied (the first, with none): a
/// step of `PRESETS - 1` is the one before it (the last, with none).
pub(crate) fn step_preset(model: &mut Model, window: WindowId, step: usize) {
    let index = match model.windows[&window].layout.last_preset() {
        Some(last) => (last + step) % PRESETS,
        None if step == 1 => 0,
        None => PRESETS - 1,
    };
    apply_nth_preset(model, window, index);
}

/// A tmux layout string (spec §4.3), which puts every pane, floating ones
/// too, into its cells, as tmux 3.7b does. tmux also resizes the window to
/// the layout's size; a client showing the session sizes it back, and with
/// none it keeps that size.
fn layout_from_string(state: &mut State, window: WindowId, name: &str) -> Result<(), String> {
    let model = &mut state.model;
    let session = model
        .link_session(window, None)
        .unwrap_or(model.windows[&window].session);
    let shown = state.clients.values().any(|cl| cl.session == session);
    let model = &mut state.model;
    let applied = model.windows.get_mut(&window).map(|w| {
        let panes = w.pane_ids();
        let applied = w.layout.apply_tmux_layout(name, &panes);
        if applied.is_ok() {
            w.floating.clear();
            if !shown && let Some((cols, rows)) = w.layout.exact_size() {
                (w.cols, w.rows) = crate::types::cap_size(cols, rows);
            }
        }
        applied
    });
    model.relayout(window);
    match applied {
        Some(Err(cause)) => Err(format!("{cause}: {name}")),
        _ => Ok(()),
    }
}

/// `preset` over `window`'s panes in index order (D18: the old layout
/// stays when it does not fit), laid out at once. As tmux 3.7b (recorded):
/// a window with tiled panes folds its floating panes into the preset,
/// after the tiled ones, and they are tiled from then on; a window with
/// only floating panes keeps them floating.
pub(crate) fn apply_preset(model: &mut Model, window: WindowId, preset: Preset) {
    if let Some(w) = model.windows.get_mut(&window)
        && !w.tiled_ids().is_empty()
    {
        let (area, panes) = (w.area(), w.pane_ids());
        if w.layout
            .preset_bordered(area, preset, &panes, w.pane_status)
        {
            w.floating.clear();
        }
    }
    model.relayout(window);
}

/// `window`'s `tiled-layout-max-columns` (0: no limit).
pub(crate) fn tiled_max_columns(model: &Model, window: WindowId) -> u16 {
    let Some(w) = model.windows.get(&window) else {
        return 0;
    };
    let chain = options::chain(model, w.active);
    model
        .options
        .lookup("tiled-layout-max-columns", &chain)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// `window`'s `main-pane-width` and `main-pane-height`, or tmux's defaults
/// where not a size, each given the room `other-pane-width` or
/// `other-pane-height` leaves the rest (`layout_set_main_v` and `_h`).
pub(crate) fn main_sizes(model: &Model, window: WindowId) -> (MainSize, MainSize) {
    let Some(w) = model.windows.get(&window) else {
        return (MainSize::WIDTH, MainSize::HEIGHT);
    };
    let chain = options::chain(model, w.active);
    let size = |name: &str, default: MainSize| {
        model
            .options
            .lookup(name, &chain)
            .and_then(MainSize::parse)
            .unwrap_or(default)
    };
    // The cells of `main` out of `room`, the border taken off, and then the
    // main pane's size when `other` is set: `room - other` unless that
    // leaves the main pane less than it asked for.
    let adjusted = |main: MainSize, other: &str, room: u16, fallback: MainSize| {
        let cells = |m: MainSize| match m {
            MainSize::Cells(c) => u32::from(c),
            MainSize::Percent(p) => u32::from(room) * u32::from(p.min(100)) / 100,
        };
        let room = u32::from(room);
        let mainw = match main {
            MainSize::Cells(c) if u32::from(c) > room => cells(fallback),
            m => cells(m),
        };
        // `PANE_MINIMUM` is 1.
        if mainw + 1 >= room {
            return main;
        }
        let other = model
            .options
            .lookup(other, &chain)
            .and_then(MainSize::parse)
            .map_or(0, cells);
        if other == 0 || other > room || room - other < mainw {
            return main;
        }
        MainSize::Cells((room - other) as u16)
    };
    let width = size("main-pane-width", MainSize::WIDTH);
    let height = size("main-pane-height", MainSize::HEIGHT);
    (
        adjusted(
            width,
            "other-pane-width",
            w.cols.saturating_sub(1),
            MainSize::WIDTH,
        ),
        adjusted(
            height,
            "other-pane-height",
            w.rows.saturating_sub(1),
            MainSize::HEIGHT,
        ),
    )
}
