//! Popups and floating panes (TC.6, M36-R1): `display-popup`, drawn by
//! `server::popup`, and `new-pane`, a pane of the window drawn by the
//! renderer over its layout. The values and error texts are tmux 3.7b's, recorded
//! on a private server.

use crate::cmd::{DisplayPopup, NewPane, Offset, Resolve};
use crate::compat;
use crate::protocol::ClientCtx;
use crate::server::dispatch::pane::print_pane;
use crate::server::dispatch::{command_argv, resolve_cwd, spawn_pane};
use crate::server::model::Floating;
use crate::server::pane::{PaneSpawn, usable_cwd};
use crate::server::popup::{self, Frame, Lines, Position};
use crate::server::status::Style;
use crate::server::{State, clients, hooks, prompt};
use crate::types::{ClientId, PaneId, Rect, SplitSize};

/// `display-popup` on the current client, or the one `-c` names: the
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
    let lines = match (c.no_border, c.border.as_deref()) {
        (true, _) => None,
        (false, None) => Some(Lines::Single),
        (false, Some(value)) => Lines::parse(value)?,
    };
    let (cols, rows) = state.clients[&id].size;
    let w = c.width.map_or(cols / 2, |s| cells(s, cols));
    let h = c.height.map_or(rows / 2, |s| cells(s, rows));
    if w > cols {
        return Err("width too large".to_string());
    }
    if h > rows {
        return Err("height too large".to_string());
    }
    let x = position(state, ctx, c.x.as_deref(), Axis::X)?;
    let y = position(state, ctx, c.y.as_deref(), Axis::Y)?;
    if state.popups.is_open(id) {
        return Ok(String::new());
    }
    let session = state.clients[&id].session;
    let pane = client_pane(state, id).ok_or("no current client")?;
    let pane_rect = clients::overlay_ctx(state, id)
        .and_then(|cx| cx.panes.iter().find(|(p, _)| *p == pane).map(|(_, r)| *r))
        .unwrap_or(Rect {
            x: 0,
            y: 0,
            cols,
            rows,
        });
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
    let frame = Frame {
        at: (i32::from(at.0), i32::from(at.1)),
        size: (w, h),
        lines,
        title,
        border_style: Style::default(),
        close_on_exit: c.close_on_exit,
    };
    // ponytail: without a command tmux starts its default-shell as a plain
    // shell; grofe starts the user's login shell, as for a pane.
    let spawn = spawn_for(state, session, pane, command_argv(c.command), cwd, env);
    if popup::open(state, id, frame, spawn)? && ctx.client.is_none() {
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
// ponytail: the pointer does not pick a floating pane (mouse.rs still
// maps a click to the tiled pane under it), and the layout verbs
// (resize-pane, swap-pane, select-layout) do not move one.
pub fn new_pane(state: &mut State, ctx: &ClientCtx, c: NewPane) -> Result<String, String> {
    let model = &state.model;
    let target = model.resolve_pane(&c.target, ctx)?;
    let window = model.panes[&target].window;
    let w = &model.windows[&window];
    let session = w.session;
    let (sx, sy) = (w.cols, w.rows);
    let width = c.width.map_or(sx / 2, |s| cells(s, sx));
    let height = c.height.map_or(sy / 4, |s| cells(s, sy));
    let n = w.floating.len() as i32 + 1;
    let x = c.x.map_or(4 * n, |o| offset(o, sx));
    let y = c.y.map_or(2 * n, |o| offset(o, sy));
    if width > sx || height > sy || x > i32::from(sx) || y > i32::from(sy) {
        return Err("size or position too large".to_string());
    }
    if x + i32::from(width) <= 0 || y + i32::from(height) <= 0 {
        return Err("size or position too small".to_string());
    }
    let s = &model.sessions[&session];
    let env = s.env.clone();
    let cwd = resolve_cwd(model, ctx, Some(target), c.cwd.as_deref(), &s.cwd);
    let cell_px = clients::cell_px(state, session);
    let new = spawn_pane(
        state,
        (session, window),
        env,
        command_argv(c.command),
        cwd,
        (width.max(1), height.max(1)),
        cell_px,
    )?;
    let Some(w) = state.model.windows.get_mut(&window) else {
        state.model.panes.remove(&new);
        return Err(format!("can't find window: {window}"));
    };
    let z = w.floating.iter().map(|f| f.z).max().unwrap_or(0) + 1;
    w.floating.push(Floating { pane: new, x, y, z });
    w.set_active(new);
    if !c.print {
        return Ok(String::new());
    }
    Ok(print_pane(&state.model, new, c.format.as_deref()))
}

/// The spawn for a popup's command: `GROFE_PANE` is `pane`'s id, `env` the
/// whole environment. The popup sets the size.
fn spawn_for(
    state: &State,
    session: crate::types::SessionId,
    pane: PaneId,
    argv: Option<Vec<String>>,
    cwd: std::path::PathBuf,
    env: Vec<(String, String)>,
) -> PaneSpawn {
    PaneSpawn {
        id: pane,
        session,
        argv,
        cwd,
        env,
        cols: 0,
        rows: 0,
        history_limit: state.model.history_limit(session),
        socket: state.socket.clone(),
        term: state.inner_term,
        cell_px: clients::cell_px(state, session),
        graphics_ok: false,
    }
}

/// The active pane of client `id`'s current window.
fn client_pane(state: &State, id: ClientId) -> Option<PaneId> {
    let s = state.model.sessions.get(&state.clients.get(&id)?.session)?;
    Some(state.model.windows.get(&s.active)?.active)
}

/// The client `-c` names, by its `client_name`, as tmux words a miss.
fn client_named(state: &State, name: &str) -> Result<ClientId, String> {
    state
        .clients
        .iter()
        .find(|(_, c)| c.name == name)
        .map(|(id, _)| *id)
        .ok_or_else(|| format!("can't find client: {name}"))
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

/// A `-x`/`-y` value: tmux's letters (`C` either way, `P`, `R` for `x`),
/// else a format expanded and read as C's `strtol` reads it. The mouse
/// letters are refused: grofe's mouse model passes the pointer as cells
/// (TC.10, spec §4.5), as for `display-menu`.
fn position(
    state: &State,
    ctx: &ClientCtx,
    value: Option<&str>,
    axis: Axis,
) -> Result<Position, String> {
    let Some(value) = value else {
        return Ok(Position::Centre);
    };
    match (value, axis) {
        ("C", _) => return Ok(Position::Centre),
        ("P", _) => return Ok(Position::Pane),
        ("R", Axis::X) => return Ok(Position::PaneRight),
        ("M" | "W", _) | ("S", Axis::Y) => {
            return Err(format!("position needs the mouse: {value}"));
        }
        _ => {}
    }
    Ok(Position::Cells(strtol(&prompt::expand_command(
        state, ctx, value,
    ))))
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
