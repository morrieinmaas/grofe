//! Mouse: click to focus, the read-only scrollback view, forwarding to
//! panes that asked for the mouse (T3.2, spec §4.11 "Mouse"; M5 replaces
//! the view with copy mode).
//!
//! As tmux 3.7b's stock root bindings, with `mouse on`:
//! - a button press focuses the pane under the pointer (`select-pane -t =`)
//!   and, if that pane asked for the mouse, is forwarded too;
//! - the wheel goes to a pane that asked for the mouse; on the alternate
//!   screen without mouse mode it does nothing (recorded: tmux sends such a
//!   pane nothing, mode 1007 or not); otherwise wheel up opens the view;
//! - in the view the wheel scrolls it 5 lines a tick and selects the pane
//!   (copy mode's `select-pane \; send -X -N 5 scroll-up`), and scrolling
//!   back to the bottom closes it (`copy-mode -e`);
//! - `q` and Escape close the view and are eaten; any other key or a paste
//!   closes it and is then routed as usual (R17);
//! - every other mouse event reaches a pane that asked for it, filtered by
//!   the mode it asked for, and nothing else.
//!
//! One divergence, from the task's acceptance case: tmux's first wheel tick
//! only enters copy mode (`[0/N]`), grofe's already scrolls 5 lines.

use std::collections::BTreeMap;
use std::time::Instant;

use alacritty_terminal::vte::ansi::{Color, NamedColor};

use crate::server::State;
use crate::server::clients;
use crate::server::input::{
    Input, InputHook, KeyCode, Mods, Mouse, MouseButton, MouseKind, Routed,
};
use crate::server::model::Window;
use crate::server::pane::{CellFlags, SnapCell};
use crate::server::render::{Overlay, OverlayCtx, Screen};
use crate::types::{ClientId, InputModes, MouseEncoding, MouseMode, PaneId, Rect};

/// Lines per wheel tick in the view (tmux's `-N 5`).
pub const WHEEL_LINES: usize = 5;

/// The panes whose scrollback view is open, by pane.
#[derive(Debug, Default)]
pub struct MouseState {
    views: BTreeMap<PaneId, View>,
}

/// An open view: `offset` lines up from the live screen, as of when the
/// pane had `history` lines of scrollback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct View {
    offset: usize,
    history: usize,
}

impl MouseState {
    /// How far `pane`'s view is scrolled back now that it has `history`
    /// lines, if it is open. Lines added since the last scroll push the
    /// offset up with them, so the view keeps showing the same lines (tmux
    /// copy mode works on a copy taken on entry).
    // ponytail: once history is at its limit, new lines evict old ones
    // without growing it, and the view drifts with the output.
    pub fn offset(&self, pane: PaneId, history: usize) -> Option<usize> {
        let v = self.views.get(&pane)?;
        let grown = history.saturating_sub(v.history);
        Some((v.offset + grown).min(history))
    }

    /// Scrolls `pane`'s view by `lines` (up is positive), opening it if
    /// needed; at or below the bottom it closes. Whether the view is open
    /// afterwards.
    pub fn scroll(&mut self, pane: PaneId, history: usize, lines: isize) -> bool {
        let now = self.offset(pane, history).unwrap_or(0);
        let offset = now.saturating_add_signed(lines).min(history);
        if offset == 0 {
            self.views.remove(&pane);
            return false;
        }
        self.views.insert(pane, View { offset, history });
        true
    }

    /// Closes `pane`'s view; whether one was open.
    pub fn close(&mut self, pane: PaneId) -> bool {
        self.views.remove(&pane).is_some()
    }

    pub fn is_open(&self, pane: PaneId) -> bool {
        self.views.contains_key(&pane)
    }
}

/// Routes mouse input (focus, wheel, forwarding) and the keys that leave
/// the view.
pub struct MouseHook;

impl InputHook for MouseHook {
    fn on_input(
        &self,
        state: &mut State,
        client: ClientId,
        input: &Input,
        now: Instant,
    ) -> Option<Routed> {
        let model = &state.model;
        state.mouse.views.retain(|p, _| model.panes.contains_key(p));
        match input {
            Input::Mouse(m) => on_mouse(state, client, m, now),
            Input::Key(key) => {
                let pane = active_pane(state, client)?;
                if !state.mouse.close(pane) {
                    return None;
                }
                clients::mark_all_dirty(state);
                let leaves = key.mods == Mods::default()
                    && matches!(key.code, KeyCode::Char('q') | KeyCode::Escape);
                leaves.then_some(Routed::Consumed)
            }
            Input::Paste { .. } => {
                let pane = active_pane(state, client)?;
                if state.mouse.close(pane) {
                    clients::mark_all_dirty(state);
                }
                None
            }
            Input::Focus(_) => None,
        }
    }
}

/// The active window of client `id`'s session.
fn client_window(state: &State, id: ClientId) -> Option<&Window> {
    let c = state.clients.get(&id)?;
    let s = state.model.sessions.get(&c.session)?;
    state.model.windows.get(&s.active)
}

fn active_pane(state: &State, id: ClientId) -> Option<PaneId> {
    client_window(state, id).map(|w| w.active)
}

/// The visible panes of `w` and their full rects in window coordinates
/// (only the active pane when zoomed), as the renderer lays them out.
fn pane_rects(w: &Window) -> Vec<(PaneId, Rect)> {
    if w.zoomed {
        vec![(w.active, w.area())]
    } else {
        w.layout.compute(w.area())
    }
}

/// The pane under client cell `(col, row)` and the 0-based cell inside it:
/// the client cell moves through the viewport (D6 crop) into window
/// coordinates; the status row, borders and letterbox belong to no pane.
fn pane_at(
    panes: &[(PaneId, Rect)],
    viewport: (u16, u16),
    status_row: Option<u16>,
    (col, row): (u16, u16),
) -> Option<(PaneId, u16, u16)> {
    if status_row.is_some_and(|s| row >= s) {
        return None;
    }
    let x = u32::from(col) + u32::from(viewport.0);
    let y = u32::from(row) + u32::from(viewport.1);
    panes.iter().find_map(|&(pane, r)| {
        let (left, top) = (u32::from(r.x), u32::from(r.y));
        let inside = (left..left + u32::from(r.cols)).contains(&x)
            && (top..top + u32::from(r.rows)).contains(&y);
        inside.then(|| (pane, (x - left) as u16, (y - top) as u16))
    })
}

fn on_mouse(state: &mut State, client: ClientId, m: &Mouse, now: Instant) -> Option<Routed> {
    let session = state.clients.get(&client)?.session;
    let on = state.model.options.lookup(
        "mouse",
        &[crate::server::dispatch::options::Owner::Session(session)],
    ) == Some("on");
    if !on {
        // The key tables drop it, as with no mouse at all.
        return None;
    }
    let cx = clients::overlay_ctx(state, client)?;
    let rects = pane_rects(client_window(state, client)?);
    // ponytail: status-line clicks, border drags and bindable mouse keys
    // are TC.10's; a drag leaving its pane is dropped rather than
    // following the pane it started in.
    let Some((pane, x, y)) = pane_at(&rects, cx.viewport, cx.status_row, (m.col, m.row)) else {
        return Some(Routed::Consumed);
    };
    let p = state.model.panes.get(&pane)?;
    let modes = p.handle.modes();
    let history = p.handle.history_size();
    let in_view = state.mouse.is_open(pane);
    let wants = modes.mouse != MouseMode::Off && !in_view;
    let select = || {
        Routed::Command(vec![vec![
            "select-pane".into(),
            "-t".into(),
            pane.to_string(),
        ]])
    };
    let wheel = match m.kind {
        MouseKind::WheelUp => Some(WHEEL_LINES as isize),
        MouseKind::WheelDown => Some(-(WHEEL_LINES as isize)),
        _ => None,
    };
    if let Some(lines) = wheel
        && !wants
    {
        if in_view {
            state.mouse.scroll(pane, history, lines);
            clients::mark_all_dirty(state);
            clients::request_render(state, now);
            return Some(select());
        }
        if lines > 0 && !modes.alt_screen && history > 0 {
            state.mouse.scroll(pane, history, lines);
            clients::mark_all_dirty(state);
            clients::request_render(state, now);
        }
        return Some(Routed::Consumed);
    }
    if wants && let Some(bytes) = encode(m, x, y, &modes) {
        let p = state.model.panes.get(&pane)?;
        if p.handle.enqueue_input(bytes).is_err() {
            eprintln!("grofe server: input queue full for {pane}");
        }
    }
    Some(match m.kind {
        MouseKind::Down(_) => select(),
        _ => Routed::Consumed,
    })
}

/// Encodes `m` at 0-based pane cell `(x, y)` for a pane with `modes`, as
/// tmux's `input_key_mouse`: `None` when the pane's mouse mode does not
/// report this kind of event, or X10 cannot carry the position.
pub fn encode(m: &Mouse, x: u16, y: u16, modes: &InputModes) -> Option<Vec<u8>> {
    let reported = match m.kind {
        MouseKind::Drag(_) => matches!(modes.mouse, MouseMode::Drag | MouseMode::Motion),
        MouseKind::Move => modes.mouse == MouseMode::Motion,
        _ => modes.mouse != MouseMode::Off,
    };
    if !reported {
        return None;
    }
    let button = |b: MouseButton| match b {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        MouseButton::Other(n) => 128 + u32::from(n.saturating_sub(8) & 3),
    };
    let sgr = modes.mouse_encoding == MouseEncoding::Sgr;
    let mut b = match m.kind {
        MouseKind::Down(btn) => button(btn),
        // Legacy encodings cannot say which button went up.
        MouseKind::Up(btn) if sgr => button(btn),
        MouseKind::Up(_) => 3,
        MouseKind::Drag(btn) => 32 + button(btn),
        MouseKind::Move => 35,
        MouseKind::WheelUp => 64,
        MouseKind::WheelDown => 65,
        MouseKind::WheelLeft => 66,
        MouseKind::WheelRight => 67,
    };
    if m.mods.shift {
        b |= 4;
    }
    if m.mods.alt {
        b |= 8;
    }
    if m.mods.ctrl {
        b |= 16;
    }
    let (x, y) = (u32::from(x) + 1, u32::from(y) + 1);
    match modes.mouse_encoding {
        MouseEncoding::Sgr => {
            let end = if matches!(m.kind, MouseKind::Up(_)) {
                'm'
            } else {
                'M'
            };
            Some(format!("\x1b[<{b};{x};{y}{end}").into_bytes())
        }
        MouseEncoding::X10 => {
            let byte = |v: u32| u8::try_from(v + 32).ok();
            Some(vec![0x1b, b'[', b'M', byte(b)?, byte(x)?, byte(y)?])
        }
        MouseEncoding::Utf8 => {
            let mut out = b"\x1b[M".to_vec();
            for v in [b, x, y] {
                let c = char::from_u32(v + 32).filter(|c| (*c as u32) < 0x800)?;
                let mut buf = [0; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
            Some(out)
        }
    }
}

/// Draws each visible pane's open view: its history rows over the pane,
/// and the `[offset/history]` indicator top right in tmux's `mode-style`
/// (black on yellow). The cursor hides while the client's active pane is
/// scrolled back.
// ponytail: tmux shows a copy-mode cursor it can move; the view has none.
pub struct ScrollbackOverlay;

impl Overlay for ScrollbackOverlay {
    fn draw(&self, state: &State, client: ClientId, cx: &OverlayCtx, screen: &mut Screen) {
        if state.mouse.views.is_empty() {
            return;
        }
        let Some(w) = client_window(state, client) else {
            return;
        };
        for (pane, rect) in pane_rects(w) {
            let Some(p) = state.model.panes.get(&pane) else {
                continue;
            };
            let history = p.handle.history_size();
            let offset = match state.mouse.offset(pane, history) {
                Some(n) if n > 0 => n,
                _ => continue,
            };
            let Some(&(_, clip)) = cx.panes.iter().find(|(id, _)| *id == pane) else {
                continue;
            };
            let mut put = |x: u16, y: u16, cell: &SnapCell| {
                let cx_ = i32::from(rect.x) + i32::from(x) - i32::from(cx.viewport.0);
                let cy_ = i32::from(rect.y) + i32::from(y) - i32::from(cx.viewport.1);
                let inside = |v: i32, lo: u16, len: u16| {
                    v >= i32::from(lo) && v < i32::from(lo) + i32::from(len)
                };
                if inside(cx_, clip.x, clip.cols) && inside(cy_, clip.y, clip.rows) {
                    let i = cy_ as usize * usize::from(screen.cols) + cx_ as usize;
                    if let Some(slot) = screen.cells.get_mut(i) {
                        *slot = cell.clone();
                    }
                }
            };
            for (y, row) in p
                .handle
                .snapshot_history(offset, rect.rows)
                .iter()
                .enumerate()
            {
                for (x, cell) in row.iter().take(usize::from(rect.cols)).enumerate() {
                    put(x as u16, y as u16, cell);
                }
            }
            let label = format!("[{offset}/{history}]");
            let start = rect.cols.saturating_sub(label.len() as u16);
            for (i, c) in label.chars().take(usize::from(rect.cols)).enumerate() {
                put(start + i as u16, 0, &indicator_cell(c));
            }
            if pane == w.active {
                screen.cursor = None;
            }
        }
    }
}

fn indicator_cell(c: char) -> SnapCell {
    SnapCell {
        c,
        fg: Color::Named(NamedColor::Black),
        bg: Color::Named(NamedColor::Yellow),
        flags: CellFlags::empty(),
        zerowidth: Vec::new(),
        underline_color: None,
        hyperlink: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mouse(kind: MouseKind) -> Mouse {
        Mouse {
            kind,
            col: 0,
            row: 0,
            mods: Mods::default(),
        }
    }

    fn modes(mouse: MouseMode, mouse_encoding: MouseEncoding) -> InputModes {
        InputModes {
            mouse,
            mouse_encoding,
            ..InputModes::default()
        }
    }

    const LEFT: MouseButton = MouseButton::Left;

    /// Recorded on tmux 3.7b: what a pane gets for each mode and encoding.
    #[test]
    fn encodes_as_tmux() {
        let sgr = modes(MouseMode::Click, MouseEncoding::Sgr);
        let x10 = modes(MouseMode::Click, MouseEncoding::X10);
        let enc = |kind, x, y, m: &InputModes| encode(&mouse(kind), x, y, m);
        assert_eq!(
            enc(MouseKind::WheelUp, 9, 4, &sgr).as_deref(),
            Some(&b"\x1b[<64;10;5M"[..])
        );
        assert_eq!(
            enc(MouseKind::Down(LEFT), 2, 3, &sgr).as_deref(),
            Some(&b"\x1b[<0;3;4M"[..])
        );
        assert_eq!(
            enc(MouseKind::Up(LEFT), 2, 3, &sgr).as_deref(),
            Some(&b"\x1b[<0;3;4m"[..])
        );
        assert_eq!(
            enc(MouseKind::WheelUp, 9, 4, &x10).as_deref(),
            Some(&b"\x1b[M`*%"[..])
        );
        assert_eq!(
            enc(MouseKind::Down(LEFT), 2, 3, &x10).as_deref(),
            Some(&b"\x1b[M #$"[..])
        );
        assert_eq!(
            enc(MouseKind::Up(LEFT), 2, 3, &x10).as_deref(),
            Some(&b"\x1b[M##$"[..])
        );
        let utf8 = modes(MouseMode::Click, MouseEncoding::Utf8);
        assert_eq!(
            enc(MouseKind::Up(LEFT), 2, 3, &utf8).as_deref(),
            Some(&b"\x1b[M##$"[..])
        );
        // Past X10's reach: dropped; UTF-8 carries it in two bytes.
        assert_eq!(enc(MouseKind::Down(LEFT), 300, 3, &x10), None);
        assert_eq!(
            enc(MouseKind::Down(LEFT), 200, 3, &utf8).as_deref(),
            Some(&b"\x1b[M \xc3\xa9$"[..])
        );
        let mut ctrl_shift = mouse(MouseKind::Down(LEFT));
        ctrl_shift.mods.ctrl = true;
        ctrl_shift.mods.shift = true;
        assert_eq!(
            encode(&ctrl_shift, 2, 3, &sgr).as_deref(),
            Some(&b"\x1b[<20;3;4M"[..])
        );
    }

    /// Mode 1000 reports presses, releases and the wheel; 1002 adds drags;
    /// 1003 adds motion with no button; off reports nothing.
    #[test]
    fn filters_by_mouse_mode() {
        let at = |mode, kind| encode(&mouse(kind), 4, 3, &modes(mode, MouseEncoding::Sgr));
        assert_eq!(at(MouseMode::Off, MouseKind::WheelUp), None);
        assert_eq!(at(MouseMode::Click, MouseKind::Drag(LEFT)), None);
        assert_eq!(at(MouseMode::Click, MouseKind::Move), None);
        assert_eq!(
            at(MouseMode::Drag, MouseKind::Drag(LEFT)).as_deref(),
            Some(&b"\x1b[<32;5;4M"[..])
        );
        assert_eq!(at(MouseMode::Drag, MouseKind::Move), None);
        assert_eq!(
            at(MouseMode::Motion, MouseKind::Move).as_deref(),
            Some(&b"\x1b[<35;5;4M"[..])
        );
    }

    fn rect(x: u16, y: u16, cols: u16, rows: u16) -> Rect {
        Rect { x, y, cols, rows }
    }

    #[test]
    fn maps_a_pointer_through_the_viewport_to_a_pane() {
        let panes = [
            (PaneId(1), rect(0, 0, 59, 23)),
            (PaneId(2), rect(60, 0, 20, 23)),
        ];
        assert_eq!(
            pane_at(&panes, (0, 0), Some(23), (3, 2)),
            Some((PaneId(1), 3, 2))
        );
        assert_eq!(
            pane_at(&panes, (0, 0), Some(23), (65, 2)),
            Some((PaneId(2), 5, 2))
        );
        // The border column, the status row, a letterbox cell.
        assert_eq!(pane_at(&panes, (0, 0), Some(23), (59, 2)), None);
        assert_eq!(pane_at(&panes, (0, 0), Some(23), (3, 23)), None);
        assert_eq!(pane_at(&panes, (0, 0), None, (3, 30)), None);
        // Cropped at x = 40: client column 30 is window column 70.
        assert_eq!(
            pane_at(&panes, (40, 1), Some(11), (30, 2)),
            Some((PaneId(2), 10, 3))
        );
        assert_eq!(pane_at(&panes, (40, 1), Some(11), (30, 11)), None);
    }

    #[test]
    fn the_view_scrolls_closes_at_the_bottom_and_stays_on_its_lines() {
        let mut s = MouseState::default();
        let p = PaneId(1);
        assert_eq!(s.offset(p, 100), None);
        assert!(s.scroll(p, 100, 5));
        assert_eq!(s.offset(p, 100), Some(5));
        assert!(s.scroll(p, 100, 5));
        assert_eq!(s.offset(p, 100), Some(10));
        // Three new lines: the same lines are now 13 up.
        assert_eq!(s.offset(p, 103), Some(13));
        assert!(s.scroll(p, 103, -5));
        assert_eq!(s.offset(p, 103), Some(8));
        assert!(!s.scroll(p, 103, -10));
        assert_eq!(s.offset(p, 103), None);
        // Capped at the history.
        assert!(s.scroll(p, 7, 5));
        assert!(s.scroll(p, 7, 5));
        assert_eq!(s.offset(p, 7), Some(7));
        assert!(s.close(p));
        assert!(!s.close(p));
    }
}
