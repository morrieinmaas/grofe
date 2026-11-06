//! Popups and floating panes (TC.6, M36-R1): `display-popup`, drawn by
//! `server::popup`, and `new-pane`, a pane of the window drawn by the
//! renderer over its layout. The values and error texts are tmux 3.7b's, recorded
//! on a private server.

use crate::cmd::{DisplayPopup, FloatPane, NewPane, Offset, PlacePane, Resolve, UnfloatPane};
use crate::compat;
use crate::protocol::ClientCtx;
use crate::server::dispatch::pane::print_pane;
use crate::server::dispatch::{resolve_cwd, spawn_pane};
use crate::server::layout::Insert;
use crate::server::model::{Floating, HOLE};
use crate::server::pane::{PaneSpawn, pane_shell, usable_cwd};
use crate::server::popup::{self, Frame, Lines, Position};
use crate::server::{State, clients, hooks, menu, prompt};
use crate::types::{ClientId, Dir, PaneId, Rect, SplitSize};

/// `display-popup -C` closes the client's popup (or menu) and nothing else;
/// on a client that has a popup open the command changes that popup
/// ([`popup::modify`]) and opens none. Otherwise, on the current client, or
/// the one `-c` names: the
/// command (the user's shell without one) in a pty inside a box `-w` by
/// `-h` (half the client each way by default), border included, placed by
/// `-x`/`-y` as a menu is. `-T` and `-d` are formats. The command gets the
/// environment a `run-shell` gets plus `TMUX` (no `TMUX_PANE`, as tmux
/// 3.7b gives a popup) and `-e`'s variables, starts in `-d` or the session's directory, and
/// `GROFE_PANE` names the client's current pane, so grofe commands run from
/// the popup act where the popup was opened. From the CLI the reply waits
/// until the popup closes, with its command's exit status.
pub fn display_popup(
    state: &mut State,
    ctx: &ClientCtx,
    c: DisplayPopup,
) -> Result<String, String> {
    let id = match &c.client {
        Some(name) => client_named(state, name)?,
        None => clients::current_client(state, ctx).ok_or("no current client")?,
    };
    if c.close {
        popup::close(state, id);
        menu::clear(state, id);
        return Ok(String::new());
    }
    let modify = state.popups.is_open(id);
    if !modify && state.menus.is_open(id) {
        return Ok(String::new());
    }
    // `-t` is the pane the formats expand against and `P` is placed by.
    let pane = match &c.target {
        Some(target) => state.model.resolve_pane(target, ctx)?,
        None => client_pane(state, id).ok_or("no current client")?,
    };
    let at_pane = ClientCtx {
        pane: Some(pane),
        ..ctx.clone()
    };
    let ctx = &at_pane;
    let lines = match (c.no_border, c.border.as_deref()) {
        (true, _) => Some(None),
        (false, None) => None,
        (false, Some(value)) => Some(Lines::parse(value)?),
    };
    if modify {
        let title = c
            .title
            .as_deref()
            .map_or_else(String::new, |t| prompt::expand_command(state, ctx, t));
        let change = popup::Change {
            title: Some(title),
            style: c.style,
            border_style: c.border_style,
            lines,
            flags: (c.reset_flags || c.close_on_exit || c.any_key).then_some((
                c.close_on_exit,
                c.close_on_zero,
                c.any_key,
            )),
        };
        popup::modify(state, id, change);
        return Ok(String::new());
    }
    let (cols, rows) = state.clients[&id].size;
    let w = c.width.map_or(cols / 2, |s| cells(s, cols));
    let h = c.height.map_or(rows / 2, |s| cells(s, rows));
    if w > cols {
        return Err("width too large".to_string());
    }
    if h > rows {
        return Err("height too large".to_string());
    }
    let session = state.clients[&id].session;
    // `popup-border-lines` is the default `-b` and `-B` override.
    let chain = crate::server::dispatch::options::chain(&state.model, pane);
    let option = |name: &str| state.model.options.lookup(name, &chain).map(str::to_string);
    let lines = match lines {
        Some(lines) => lines,
        None => Lines::parse(&option("popup-border-lines").unwrap_or_default())
            .unwrap_or(Some(Lines::Single)),
    };
    let pane_rect = clients::overlay_ctx(state, id)
        .and_then(|cx| cx.panes.iter().find(|(p, _)| *p == pane).map(|(_, r)| *r))
        .unwrap_or(Rect {
            x: 0,
            y: 0,
            cols,
            rows,
        });
    let spot = Spot {
        id,
        size: (w, h),
        client: (cols, rows),
        pane: pane_rect,
    };
    let x = position(state, ctx, c.x.as_deref(), Axis::X, &spot)?;
    let y = position(state, ctx, c.y.as_deref(), Axis::Y, &spot)?;
    let at = popup::place((x, y), (w, h), (cols, rows), pane_rect);
    let title = c
        .title
        .as_deref()
        .map(|t| prompt::expand_command(state, ctx, t));
    let model = &state.model;
    let default_cwd = model.sessions[&session].cwd.clone();
    let dir = c
        .cwd
        .as_deref()
        .map(|d| prompt::expand_command(state, ctx, d));
    let cwd = usable_cwd(dir.map_or(default_cwd, |d| ctx.cwd.join(d)));
    let mut env = hooks::shell_env(state, Some(session));
    env.extend(
        compat::pane_env(state, session, pane)
            .into_iter()
            .filter(|(k, _)| k != "TMUX_PANE"),
    );
    env.extend(c.env);
    // As tmux's `popup_display` (`JOB_DEFAULTSHELL`): `default-shell` runs a
    // one-word command, else `default-command`, else starts as itself, and
    // is exported as `SHELL` last.
    let shell = pane_shell(model.default_shell(session).as_deref());
    env.push(("SHELL".to_string(), shell.clone()));
    let argv = match c.command.filter(|a| !a.is_empty() && a != &[""]) {
        Some(argv) => Some(argv),
        None => model.default_command(session).map(|cmd| vec![cmd]),
    };
    let frame = Frame {
        at: (i32::from(at.0), i32::from(at.1)),
        size: (w, h),
        lines,
        title,
        border_style: c.border_style,
        style: c.style,
        close_on_exit: c.close_on_exit,
        close_on_zero: c.close_on_zero,
        any_key: c.any_key,
    };
    let spawn = spawn_for(state, session, pane, (argv, shell), cwd, env);
    if popup::open(state, id, frame, spawn)? {
        popup::wait_for_close(state, id);
    }
    Ok(String::new())
}

/// `new-pane`: tmux 3.7's floating pane, a pane of the target pane's
/// window outside its layout, drawn over it on every client showing the
/// window (tmux 3.7b, recorded). `-x`/`-y` are its size (half the
/// window's width and a quarter of its height by default) and `-X`/`-Y`
/// the window cell of its top-left content cell, cells or a percent of
/// the window; by default each new one cascades 4 right and 2 down. It
/// becomes the active pane, takes input and bindings as any pane does,
/// is listed and counted with the window's panes, and closes as a pane
/// does. `-P` prints it (`-F`, else tmux's `session:window.pane`).
// ponytail: a floating pane does not move when its window is resized.
pub fn new_pane(state: &mut State, ctx: &ClientCtx, c: NewPane) -> Result<String, String> {
    let model = &state.model;
    let target = model.resolve_pane(&c.target, ctx)?;
    let window = model.panes[&target].window;
    let w = &model.windows[&window];
    let session = w.session;
    let (x, y, width, height) = float_rect(w, (c.width, c.height), (c.x, c.y))?;
    let s = &model.sessions[&session];
    let mut env = s.env.clone();
    env.extend(c.env);
    let cwd = resolve_cwd(model, ctx, Some(target), c.cwd.as_deref(), &s.cwd);
    let cell_px = clients::cell_px(state, session);
    let new = spawn_pane(
        state,
        (session, window),
        env,
        c.command,
        cwd,
        (width, height),
        cell_px,
    )?;
    let Some(w) = state.model.windows.get_mut(&window) else {
        state.model.panes.remove(&new);
        return Err(format!("can't find window: {window}"));
    };
    let z = w.floating.iter().map(|f| f.z).max().unwrap_or(0) + 1;
    w.floating.push(Floating {
        pane: new,
        x,
        y,
        cols: width,
        rows: height,
        z,
    });
    if !c.detached {
        w.set_active(new);
    }
    crate::server::dispatch::pane::apply_setup(state, ctx, new, &c.setup)?;
    if !c.print {
        return Ok(String::new());
    }
    Ok(print_pane(&state.model, new, c.format.as_deref()))
}

/// The content rect `(x, y, cols, rows)` of a new floating pane in `w`, from
/// `new-pane`'s `-x -y -X -Y`: half the window's width and a quarter of its
/// height, cascading 4 right and 2 down, unless given.
fn float_rect(
    w: &crate::server::model::Window,
    (width, height): (Option<SplitSize>, Option<SplitSize>),
    (x, y): (Option<Offset>, Option<Offset>),
) -> Result<(i32, i32, u16, u16), String> {
    let (sx, sy) = (w.cols, w.rows);
    let width = width.map_or(sx / 2, |s| cells(s, sx));
    let height = height.map_or(sy / 4, |s| cells(s, sy));
    let n = w.floating.len() as i32 + 1;
    let x = x.map_or(4 * n, |o| offset(o, sx));
    let y = y.map_or(2 * n, |o| offset(o, sy));
    if width > sx || height > sy || x > i32::from(sx) || y > i32::from(sy) {
        return Err("size or position too large".to_string());
    }
    if x + i32::from(width) <= 0 || y + i32::from(height) <= 0 {
        return Err("size or position too small".to_string());
    }
    Ok((x, y, width.max(1), height.max(1)))
}

/// `float-pane` (grofe's own): the target tiled pane leaves the layout for
/// a floating place, on top, with its process and history. The window needs
/// another tiled pane to keep the layout.
pub fn float_pane(state: &mut State, ctx: &ClientCtx, c: FloatPane) -> Result<String, String> {
    let model = &mut state.model;
    let pane = model.resolve_pane(&c.target, ctx)?;
    let window = model.panes[&pane].window;
    let w = &model.windows[&window];
    if w.floating(pane).is_some() {
        return Err("pane is already floating".to_string());
    }
    if w.tiled_ids().len() < 2 {
        return Err("can't float the only tiled pane".to_string());
    }
    let (x, y, cols, rows) = float_rect(w, (c.width, c.height), (c.x, c.y))?;
    let Some(w) = model.windows.get_mut(&window) else {
        return Ok(String::new());
    };
    // The pane stays the window's, so the active and last panes stand.
    w.zoomed = false;
    w.layout.remove(pane);
    let z = w.floating.iter().map(|f| f.z).max().unwrap_or(0) + 1;
    w.floating.push(Floating {
        pane,
        x,
        y,
        cols,
        rows,
        z,
    });
    model.relayout(window);
    Ok(String::new())
}

/// `unfloat-pane` (grofe's own): the floating target pane goes back into
/// the layout with its process and history, splitting `-s`, else the tiled
/// pane under its centre, else the window's latest active tiled pane, else
/// its first one; a window with no tiled pane left takes it as its only
/// one. The split is `-h`/`-v`, else along the longer side of the pane
/// split. The pane becomes the active one.
pub fn unfloat_pane(state: &mut State, ctx: &ClientCtx, c: UnfloatPane) -> Result<String, String> {
    let model = &mut state.model;
    let pane = model.resolve_pane(&c.target, ctx)?;
    let window = model.panes[&pane].window;
    let w = &model.windows[&window];
    let Some(f) = w.floating(pane).copied() else {
        return Err("pane is not floating".to_string());
    };
    let mut rects = w.tiled_rects();
    rects.retain(|(id, _)| *id != HOLE);
    let rect_of = |p: PaneId| rects.iter().find(|(id, _)| *id == p).map(|(_, r)| *r);
    let split = match &c.split {
        Some(t) => {
            let t = model.resolve_pane(t, ctx)?;
            if rect_of(t).is_none() {
                return Err("target pane is not a tiled pane of the window".to_string());
            }
            Some(t)
        }
        None => {
            let (cx, cy) = (f.x + i32::from(f.cols / 2), f.y + i32::from(f.rows / 2));
            let under = rects.iter().find(|(_, r)| {
                (i32::from(r.x)..i32::from(r.x) + i32::from(r.cols)).contains(&cx)
                    && (i32::from(r.y)..i32::from(r.y) + i32::from(r.rows)).contains(&cy)
            });
            under
                .map(|(id, _)| *id)
                .or_else(|| {
                    // The active pane, if tiled, is the latest active one.
                    std::iter::once(w.active)
                        .chain(w.last_panes.iter().rev().copied())
                        .find(|p| rect_of(*p).is_some())
                })
                .or_else(|| rects.first().map(|(id, _)| *id))
        }
    };
    let mut layout = w.layout.clone();
    match split.and_then(|t| rect_of(t).map(|r| (t, r))) {
        Some((t, r)) => {
            let dir = c.dir.unwrap_or(if r.cols > r.rows {
                Dir::Horizontal
            } else {
                Dir::Vertical
            });
            let how = Insert {
                dir,
                size: SplitSize::Half,
                before: false,
                full: false,
            };
            layout
                .insert(w.area(), t, pane, how)
                .map_err(|_| "no space for a new pane".to_string())?;
        }
        // Only floating panes are left: the pane takes the layout's hole.
        None => {
            layout.replace(HOLE, pane);
        }
    }
    crate::server::drop::apply(model, window, pane, layout);
    Ok(String::new())
}

/// `place-pane` (grofe's own): the floating target pane moves to `-X`, `-Y`.
pub fn place_pane(state: &mut State, ctx: &ClientCtx, c: PlacePane) -> Result<String, String> {
    let model = &mut state.model;
    let pane = model.resolve_pane(&c.target, ctx)?;
    let window = model.panes[&pane].window;
    let Some(w) = model.windows.get_mut(&window) else {
        return Ok(String::new());
    };
    let size = (w.cols, w.rows);
    let Some(f) = w.floating.iter_mut().find(|f| f.pane == pane) else {
        return Err("pane is not floating".to_string());
    };
    if c.x.is_none() && c.y.is_none() {
        return Err("place-pane needs -X or -Y".to_string());
    }
    let x = c.x.map_or(f.x, |o| offset(o, size.0));
    let y = c.y.map_or(f.y, |o| offset(o, size.1));
    let at = crate::server::floating::placed(f, (x, y), size);
    (f.x, f.y) = (at.x, at.y);
    Ok(String::new())
}

/// The spawn for a popup's command: `GROFE_PANE` is `pane`'s id, `env` the
/// whole environment. The popup sets the size.
pub(crate) fn spawn_for(
    state: &State,
    session: crate::types::SessionId,
    pane: PaneId,
    (argv, shell): (Option<Vec<String>>, String),
    cwd: std::path::PathBuf,
    env: Vec<(String, String)>,
) -> PaneSpawn {
    PaneSpawn {
        id: pane,
        session,
        argv,
        shell,
        login: false,
        cwd,
        env,
        cols: 0,
        rows: 0,
        history_limit: state.model.history_limit(session),
        socket: state.socket.clone(),
        term: state.pane_term(),
        cell_px: clients::cell_px(state, session),
        graphics_ok: false,
        images: false,
        identity: state.model.identity,
        delay: std::time::Duration::ZERO,
    }
}

/// The active pane of client `id`'s current window.
fn client_pane(state: &State, id: ClientId) -> Option<PaneId> {
    let s = state.model.sessions.get(&state.clients.get(&id)?.session)?;
    Some(state.model.windows.get(&s.active)?.active)
}

/// The client `-c` names, by its name or tty, as tmux words a miss.
fn client_named(state: &State, name: &str) -> Result<ClientId, String> {
    clients::client_named(state, name).ok_or_else(|| format!("can't find client: {name}"))
}

/// A size in cells of `len`: cells as given, or a percent rounded down.
fn cells(size: SplitSize, len: u16) -> u16 {
    match size {
        SplitSize::Cells(n) => n,
        SplitSize::Percent(p) => (u32::from(len) * u32::from(p) / 100) as u16,
        SplitSize::Half => len / 2,
    }
}

/// A `new-pane` position in window cells.
fn offset(o: Offset, len: u16) -> i32 {
    match o {
        Offset::Cells(n) => n,
        Offset::Percent(p) => (u32::from(len) * u32::from(p) / 100) as i32,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Axis {
    X,
    Y,
}

/// Where a popup goes: its client, its size, the client's size and the
/// current pane's rect on it.
struct Spot {
    id: ClientId,
    size: (u16, u16),
    client: (u16, u16),
    pane: Rect,
}

/// A `-x`/`-y` value: tmux's letters (`C` either way, `P`, `R` for `x`),
/// `M`, `W` and `S` as `display-menu` reads them
/// ([`menu::resolve_spec`]), else a format expanded and read as C's
/// `strtol` reads it.
fn position(
    state: &State,
    ctx: &ClientCtx,
    value: Option<&str>,
    axis: Axis,
    spot: &Spot,
) -> Result<Position, String> {
    let Some(value) = value else {
        return Ok(Position::Centre);
    };
    match (value, axis) {
        ("C", _) => return Ok(Position::Centre),
        ("P", _) => return Ok(Position::Pane),
        ("R", Axis::X) => return Ok(Position::PaneRight),
        ("M" | "W", _) | ("S", Axis::Y) => {}
        _ if value.contains("popup_") => {}
        _ => {
            return Ok(Position::Cells(strtol(&prompt::expand_command(
                state, ctx, value,
            ))));
        }
    }
    let axis = match axis {
        Axis::X => menu::Axis::X,
        Axis::Y => menu::Axis::Y,
    };
    Ok(Position::Cells(menu::resolve_spec(
        state,
        ctx,
        spot.id,
        value,
        axis,
        spot.size,
        spot.client,
        spot.pane,
    )))
}

/// C's `strtol(s, NULL, 10)`: blanks, a sign, then digits; 0 without any.
fn strtol(s: &str) -> i64 {
    let s = s.trim_start();
    let (sign, digits) = match s.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, s.strip_prefix('+').unwrap_or(s)),
    };
    let end = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    digits[..end].parse::<i64>().map_or(0, |n| sign * n)
}

#[cfg(test)]
mod tests {
    use crate::server::{dispatch, test_ctx, test_state};

    fn run(state: &mut crate::server::State, words: &[&str]) -> Result<String, String> {
        let argv: Vec<String> = words.iter().map(|w| w.to_string()).collect();
        dispatch::run(state, &test_ctx(), &argv)
    }

    /// tmux 3.7b with no client attached: `no current client`; a `-c`
    /// that names no client: `can't find client: <name>`.
    #[test]
    fn a_popup_needs_a_client() {
        let mut state = test_state();
        assert_eq!(
            run(&mut state, &["display-popup", "true"]),
            Err("no current client".to_string())
        );
        assert_eq!(
            run(&mut state, &["display-popup", "-c", "nobody", "true"]),
            Err("can't find client: nobody".to_string())
        );
    }

    #[test]
    fn new_pane_needs_its_target() {
        let mut state = test_state();
        let got = run(&mut state, &["new-pane", "-x", "10", "-y", "3", "true"]);
        assert!(got.is_err(), "{got:?}");
    }

    #[test]
    fn strtol_reads_a_leading_number() {
        assert_eq!(super::strtol(" 12x"), 12);
        assert_eq!(super::strtol("-3"), -3);
        assert_eq!(super::strtol("x"), 0);
    }
}
