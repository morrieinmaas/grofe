//! window commands, plus `list-panes` in its default format.

use std::path::Path;

use crate::cmd::{
    self, KillWindow, ListPanes, ListWindows, NewWindow, Rename, Resolve, SelectLayout,
    SelectWindow, Target, WindowRef, WindowStep,
};
use crate::protocol::ClientCtx;
use crate::server::dispatch::options::{self, FormatCtx, Owner};
use crate::server::dispatch::{command_argv, resolve_cwd, spawn_pane};
use crate::server::layout::{LayoutTree, MainSize, Preset};
use crate::server::model::{Model, Window};
use crate::server::{State, clients};
use crate::types::WindowId;

pub(crate) const AUTOMATIC_RENAME: &str = "automatic-rename";

/// A new window's name when `-n` is not given: the basename of its cwd,
/// until [`automatic_rename`] names it from its pane.
pub fn default_name(cwd: &Path) -> String {
    cwd.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| cwd.display().to_string())
}

/// `new-window`: `-t session:N` creates window N (which must be free);
/// any other target picks the session and the lowest free index.
pub fn new_window(state: &mut State, ctx: &ClientCtx, c: NewWindow) -> Result<String, String> {
    let model = &state.model;
    let (session, index) = match &c.target {
        Target::Window(s, WindowRef::Index(index)) => {
            let session = model.resolve_session(&Target::Session(s.clone()), ctx)?;
            if model.window_at_index(session, *index).is_some() {
                return Err(format!("index {index} in use"));
            }
            (session, *index)
        }
        target => {
            let session = model.resolve_session(target, ctx)?;
            (session, model.free_window_index(session))
        }
    };
    let s = &model.sessions[&session];
    let env = s.env.clone();
    let target = model.windows.get(&s.active).map(|w| w.active);
    let cwd = resolve_cwd(model, ctx, target, c.cwd.as_deref(), &s.cwd);
    let size = (s.cols, s.rows);
    let cell_px = clients::cell_px(state, session);

    let window = state.model.alloc_window_id();
    let pane = spawn_pane(
        state,
        (session, window),
        env,
        command_argv(c.command),
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
    });
    // As tmux: a window named by `-n` keeps that name.
    if named {
        stop_automatic_rename(model, window)?;
    }
    if !c.detached {
        model.select_window(window);
    }
    Ok(String::new())
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
pub fn rename_window(state: &mut State, ctx: &ClientCtx, c: Rename) -> Result<String, String> {
    let model = &mut state.model;
    let window = model.resolve_window(&c.target, ctx)?;
    stop_automatic_rename(model, window)?;
    if let Some(w) = model.windows.get_mut(&window) {
        w.name = c.name;
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
/// next or previous one with an alert, which in grofe is a bell
/// (`window_bell_flag`, R13); none is an error, as in tmux 3.7b.
pub fn step_window(
    state: &mut State,
    ctx: &ClientCtx,
    c: WindowStep,
    step: Step,
) -> Result<String, String> {
    let model = &mut state.model;
    let session = model.resolve_session(&c.target, ctx)?;
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
    let alerted = |id: &WindowId| model.windows.get(id).is_some_and(|w| w.bell_flag);
    let window = if c.alert {
        (1..n)
            .map(|k| s.windows[at(k)])
            .find(alerted)
            .ok_or_else(none)?
    } else {
        s.windows[at(1)]
    };
    model.select_window(window);
    Ok(String::new())
}

/// `last-window`: back to the target session's previous window.
pub fn last_window(state: &mut State, ctx: &ClientCtx, c: WindowStep) -> Result<String, String> {
    let model = &mut state.model;
    let session = model.resolve_session(&c.target, ctx)?;
    let s = &model.sessions[&session];
    let last = *s
        .last_windows
        .last()
        .ok_or_else(|| "no last window".to_string())?;
    model.select_window(last);
    Ok(String::new())
}

pub fn kill_window(state: &mut State, ctx: &ClientCtx, c: KillWindow) -> Result<String, String> {
    let window = state.model.resolve_window(&c.target, ctx)?;
    let removed = state.model.remove_window(window);
    state.events.panes_killed(removed);
    Ok(String::new())
}

pub fn select_window(
    state: &mut State,
    ctx: &ClientCtx,
    c: SelectWindow,
) -> Result<String, String> {
    let model = &mut state.model;
    let window = model.resolve_window(&c.target, ctx)?;
    model.select_window(window);
    Ok(String::new())
}

/// `list-windows` in the default format; `-F` is routed to `options`.
pub fn list_windows(state: &State, ctx: &ClientCtx, c: ListWindows) -> Result<String, String> {
    let model = &state.model;
    let session = model.resolve_session(&c.target, ctx)?;
    let s = &model.sessions[&session];
    Ok(s.windows
        .iter()
        .filter_map(|id| model.windows.get(id))
        .map(|w| {
            let active = w.id == s.active;
            format!(
                "{}: {}{} ({} panes) [{}x{}] {}{}\n",
                w.index,
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
    let window = model.resolve_window(&c.target, ctx)?;
    let w = &model.windows[&window];
    Ok(w.pane_ids()
        .iter()
        .zip(model.pane_base_index(window)..)
        .filter_map(|(id, index)| Some((index, model.panes.get(id)?)))
        .map(|(index, p)| {
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
    if c.spread {
        let pane = model.resolve_pane(&c.target, ctx)?;
        let window = model.panes[&pane].window;
        if let Some(w) = model.windows.get_mut(&window) {
            w.zoomed = false;
            let area = w.area();
            w.layout.spread(area, pane);
        }
        model.relayout(window);
        return Ok(String::new());
    }
    let window = model.resolve_window(&c.target, ctx)?;
    let (width, height) = main_sizes(model, window);
    let Some(w) = model.windows.get_mut(&window) else {
        return Err(format!("can't find window: {window}"));
    };
    w.zoomed = false;
    let Some(preset) = Preset::from_name(&c.name, width, height) else {
        model.relayout(window);
        return Err(format!("invalid layout: {}", c.name));
    };
    apply_preset(model, window, preset);
    Ok(String::new())
}

/// `preset` over `window`'s panes in index order (D18: the old layout
/// stays when it does not fit), laid out at once.
pub(crate) fn apply_preset(model: &mut Model, window: WindowId, preset: Preset) {
    if let Some(w) = model.windows.get_mut(&window) {
        let (area, panes) = (w.area(), w.pane_ids());
        w.layout.preset(area, preset, &panes);
    }
    model.relayout(window);
}

/// `window`'s `main-pane-width` and `main-pane-height`, or tmux's defaults
/// where unset or not a size.
// ponytail: grofe has no `main-pane-height` option yet (TC.2b registered
// the width only), so the height is always tmux's default of 24.
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
    (
        size("main-pane-width", MainSize::WIDTH),
        size("main-pane-height", MainSize::HEIGHT),
    )
}
